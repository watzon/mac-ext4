//! Mount-time orphan cleanup: the orphan file format and long orphan lists.
mod support;

use std::path::Path;
use std::sync::Arc;

use ext4_core::bmap::Mapping;
use ext4_core::crc::crc32c;
use ext4_core::inode::Inode;
use ext4_core::mkfs::{format, MkfsOptions};
use ext4_core::superblock::ro_compat;
use ext4_core::volume::Volume;
use ext4_core::{BlockDevice, Error, FileDevice, Filesystem, MemDevice, MountOptions};

/// Require `e2fsck -fn` to exit 0 and print nothing beyond the pass
/// headers and the summary.
fn assert_fsck_clean(rel: &str) {
    let out = support::e2fs(&["e2fsck", "-fn", rel]);
    let text = support::text(&out);
    assert!(out.status.success(), "e2fsck -fn {rel}:\n{text}");
    for line in text.lines() {
        let fine = line.is_empty()
            || line.starts_with("Pass ")
            || line.starts_with("e2fsck ")
            || line.contains(" files (");
        assert!(fine, "e2fsck -fn {rel} reported: {line}\n{text}");
    }
}

/// Where the first orphan file block lives and the seed of its checksum.
struct OrphanBlock {
    physical: u64,
    seed: u32,
    block_size: usize,
}

fn orphan_block(abs: &Path) -> OrphanBlock {
    let v = Volume::open(
        Box::new(FileDevice::open(abs, false).unwrap()),
        &MountOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    let ino = v.sb.orphan_file_inum();
    assert_ne!(ino, 0, "mke2fs did not create an orphan file");
    let inode = v.read_inode(ino).unwrap();
    let Mapping::Data { physical, .. } = v.map_blocks(ino, &inode, 0, 1).unwrap() else {
        panic!("orphan file block 0 is not mapped");
    };
    OrphanBlock {
        physical,
        seed: Inode::csum_seed(&v.sb, ino, inode.generation()),
        block_size: v.block_size(),
    }
}

impl OrphanBlock {
    /// Linux `ext4_orphan_file_block_csum_verify`.
    fn checksum(&self, block: &[u8]) -> u32 {
        let c = crc32c(self.seed, &self.physical.to_le_bytes());
        crc32c(c, &block[..self.block_size - 8])
    }

    fn read(&self, dev: &FileDevice) -> Vec<u8> {
        let mut block = vec![0u8; self.block_size];
        dev.read_at(self.physical * self.block_size as u64, &mut block)
            .unwrap();
        block
    }

    fn stored_checksum(&self, block: &[u8]) -> u32 {
        u32::from_le_bytes(block[self.block_size - 4..].try_into().unwrap())
    }
}

/// Make an ext4 image with an orphan file and one regular file whose inode
/// number is listed in orphan file slot 0. `valid` chooses a correct or a
/// wrong block checksum.
fn image_with_orphan_entry(
    name: &str,
    valid: bool,
) -> (std::path::PathBuf, String, OrphanBlock, u32) {
    let (abs, rel) = support::mke2fs(
        name,
        "64M",
        &["-t", "ext4", "-b", "4096", "-O", "orphan_file"],
    );
    let fs = support::mount(&abs, false);
    let ino = fs.create(2, b"linked", 0o100644, 0, 0, (0, 0)).unwrap().ino;
    fs.write(ino, 0, &[0x42; 4096]).unwrap();
    fs.unmount().unwrap();

    let ob = orphan_block(&abs);
    let dev = FileDevice::open(&abs, true).unwrap();
    let mut block = ob.read(&dev);
    assert_eq!(
        ob.stored_checksum(&block),
        ob.checksum(&block),
        "mke2fs orphan block"
    );
    block[0..4].copy_from_slice(&ino.to_le_bytes());
    let mut checksum = ob.checksum(&block);
    if !valid {
        checksum ^= 0xDEAD_BEEF;
    }
    block[ob.block_size - 4..].copy_from_slice(&checksum.to_le_bytes());
    dev.write_at(ob.physical * ob.block_size as u64, &block)
        .unwrap();
    let mut sb = ext4_core::volume::read_superblock(&dev).unwrap();
    sb.set_feature_ro_compat(sb.feature_ro_compat() | ro_compat::ORPHAN_PRESENT);
    sb.update_checksum();
    dev.write_at(1024, &sb.raw).unwrap();
    dev.flush().unwrap();
    (abs, rel, ob, ino)
}

#[test]
fn orphan_file_cleanup_writes_a_checksum_over_the_physical_block() {
    if !support::oracle_or_skip("orphan file checksum") {
        return;
    }
    let (abs, rel, ob, ino) = image_with_orphan_entry("orphan-file-checksum", true);
    let fs = support::mount(&abs, false);
    assert_eq!(fs.recovery_report().orphans_processed, 1);
    assert_eq!(fs.getattr(ino).unwrap().size, 4096);
    fs.unmount().unwrap();

    let block = ob.read(&FileDevice::open(&abs, false).unwrap());
    assert!(block[..ob.block_size - 8].iter().all(|&b| b == 0));
    assert_eq!(ob.stored_checksum(&block), ob.checksum(&block));
    assert_fsck_clean(&rel);
}

#[test]
fn orphan_file_block_with_a_bad_checksum_is_not_processed() {
    if !support::oracle_or_skip("orphan file bad checksum") {
        return;
    }
    let (abs, _, ob, ino) = image_with_orphan_entry("orphan-file-bad-checksum", false);
    let before = ob.read(&FileDevice::open(&abs, false).unwrap());
    let result = Filesystem::open(
        Box::new(FileDevice::open(&abs, true).unwrap()),
        &MountOptions::default(),
    );
    assert!(
        matches!(result, Err(Error::Corrupt(_))),
        "mount must report the corrupt orphan block"
    );
    let after = ob.read(&FileDevice::open(&abs, false).unwrap());
    assert_eq!(before, after, "the corrupt orphan block was modified");
    assert_eq!(u32::from_le_bytes(after[0..4].try_into().unwrap()), ino);
}

#[test]
fn thousands_of_orphans_are_cleaned_up_through_a_small_journal() {
    const ORPHANS: usize = 4200;
    let dev = Arc::new(MemDevice::new(24 << 20));
    let report = format(
        &*dev,
        &MkfsOptions {
            block_size: Some(1024),
            inode_ratio: Some(1024),
            ..Default::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(report.journal_blocks, 1024);
    let fs = Filesystem::open(Box::new(dev.clone()), &MountOptions::default()).unwrap();
    let free_inodes = fs.statfs().free_inodes;
    for i in 0..ORPHANS {
        fs.create(2, format!("f{i}").as_bytes(), 0o100644, 0, 0, (0, 0))
            .unwrap();
    }
    fs.sync().unwrap();
    // Unlink while "still open": the inodes become orphans.
    for i in 0..ORPHANS {
        fs.remove(2, format!("f{i}").as_bytes()).unwrap();
    }
    fs.sync().unwrap();
    // Crash with every orphan recorded on disk.
    std::mem::forget(fs);

    let crashed = MemDevice::from_vec(dev.snapshot());
    let fs = Filesystem::open(Box::new(crashed), &MountOptions::default()).unwrap();
    assert!(!fs.is_read_only(), "{:?}", fs.read_only_reason());
    assert_eq!(fs.recovery_report().orphans_processed, ORPHANS as u32);
    assert_eq!(fs.statfs().free_inodes, free_inodes);
    fs.with_volume(|v| assert_eq!(v.sb.last_orphan(), 0));
    fs.unmount().unwrap();

    if support::oracle_or_skip("orphan capacity") {
        // Same crash image as a file, cleaned up by a mount, then checked.
        let rel = "target/img/orphan-capacity.img";
        let abs = support::workspace().join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, dev.snapshot()).unwrap();
        let fs = support::mount(&abs, false);
        assert_eq!(fs.recovery_report().orphans_processed, ORPHANS as u32);
        fs.unmount().unwrap();
        assert_fsck_clean(rel);
    }
}
