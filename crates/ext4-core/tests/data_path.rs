//! Data path regressions: stale bytes past EOF, 32-bit block-map
//! allocation, `huge_file` accounting, and path-local extent updates.

mod support;

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ext4_core::group::GroupDesc;
use ext4_core::inode::Inode;
use ext4_core::mkfs::{format, MkfsOptions, Revision};
use ext4_core::ops::SetAttr;
use ext4_core::superblock::ro_compat;
use ext4_core::volume::Volume;
use ext4_core::{BlockDevice, Error, FileDevice, Filesystem, MountOptions};
use support::*;

/// Assert `e2fsck -fn` ran every pass and printed nothing else.
fn assert_fsck_silent(rel: &str) {
    let out = e2fs(&["e2fsck", "-fn", rel]);
    let text = text(&out);
    assert!(out.status.success(), "e2fsck -fn {rel} failed:\n{text}");
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let expected = line.starts_with("e2fsck ")
            || line.starts_with("Pass ")
            || (line.starts_with(rel) && line.contains(" files (") && line.ends_with(" blocks"));
        assert!(expected, "e2fsck -fn {rel} reported `{line}`:\n{text}");
    }
}

/// Format `target/img/<name>.img` with the engine's own mkfs.
fn engine_image(name: &str, bytes: u64, revision: Revision, block_size: u32) -> String {
    let rel = format!("target/img/{name}.img");
    let abs = workspace().join(&rel);
    fs::create_dir_all(abs.parent().unwrap()).unwrap();
    let _ = fs::remove_file(&abs);
    File::create(&abs).unwrap().set_len(bytes).unwrap();
    let dev = FileDevice::open(&abs, true).unwrap();
    format(
        &dev,
        &MkfsOptions {
            revision,
            block_size: Some(block_size),
            ..MkfsOptions::default()
        },
        &mut |_| {},
    )
    .unwrap();
    rel
}

fn create(fs: &Filesystem, name: &str) -> u32 {
    fs.create(2, name.as_bytes(), 0o100644, 0, 0, (0, 0))
        .unwrap()
        .ino
}

fn read_range(fs: &Filesystem, ino: u32, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let n = fs.read(ino, offset, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

fn set_size(fs: &Filesystem, ino: u32, size: u64) {
    fs.setattr(
        ino,
        &SetAttr {
            size: Some(size),
            ..Default::default()
        },
    )
    .unwrap();
}

/// Use up every free block with a filler file.
fn fill_up(fs: &Filesystem) {
    let filler = create(fs, "filler");
    let data = vec![0x33u8; 1 << 20];
    let mut off = 0u64;
    loop {
        let n = (fs.statfs().free_blocks as usize * 4096).min(data.len());
        if n == 0 {
            break;
        }
        match fs.write(filler, off, &data[..n]) {
            Ok(n) => off += n as u64,
            Err(_) => break,
        }
    }
    while fs.write(filler, off, &data[..4096]).is_ok() {
        off += 4096;
    }
}

/// A write that fails with ENOSPC after filling the old last block must
/// not leave bytes that a later extension exposes, by truncate or by a
/// write that starts past EOF.
#[test]
fn data_failed_append_never_exposes_stale_tail() {
    if !oracle_or_skip("data_failed_append_never_exposes_stale_tail") {
        return;
    }
    let (abs, rel) = mke2fs("data-eof", "64M", &["-t", "ext4", "-b", "4096"]);
    let fs = mount(&abs, false);
    let ino = create(&fs, "file");
    fs.write(ino, 0, &[0x55]).unwrap();
    fill_up(&fs);
    assert_eq!(fs.statfs().free_blocks, 0);

    // Growing by truncate.
    assert!(matches!(
        fs.write(ino, 1, &[0xA5; 8192]),
        Err(Error::NoSpace)
    ));
    assert_eq!(fs.getattr(ino).unwrap().size, 1);
    set_size(&fs, ino, 4096);
    let grown = read_range(&fs, ino, 0, 4096);
    assert_eq!(grown[0], 0x55);
    assert_eq!(
        grown[1..].iter().filter(|&&b| b != 0).count(),
        0,
        "truncate up exposed bytes of the failed append"
    );

    // Growing by a write that starts past EOF.
    set_size(&fs, ino, 1);
    assert!(matches!(
        fs.write(ino, 1, &[0xA5; 8192]),
        Err(Error::NoSpace)
    ));
    assert_eq!(fs.getattr(ino).unwrap().size, 1);
    fs.write(ino, 3000, &[0x77]).unwrap();
    let grown = read_range(&fs, ino, 0, 3001);
    assert_eq!(grown[0], 0x55);
    assert_eq!(grown[3000], 0x77);
    assert_eq!(
        grown[1..3000].iter().filter(|&&b| b != 0).count(),
        0,
        "write past EOF exposed bytes of the failed append"
    );
    set_size(&fs, ino, 4096);
    assert!(read_range(&fs, ino, 3001, 1095).iter().all(|&b| b == 0));
    fs.unmount().unwrap();
    assert_fsck_silent(&rel);
}

/// Low group 131071 ends at block 2^32 - 1; group 131072 starts at 2^32
/// and is entirely free.
const LOW_GROUP: usize = 131_071;
const HIGH_GROUP: usize = 131_072;
const BOUNDARY: u64 = 1 << 32;

/// A block-mapped file on a volume whose block count is pushed past 2^32
/// in memory.  `free_low` lists the free bits of the last group below
/// 2^32; logical block 11 of the file maps to `last_ptr`.
fn boundary_volume(name: &str, free_low: &[usize], last_ptr: u32) -> (Volume, u32, String) {
    let rel = engine_image(name, 64 << 20, Revision::Ext4, 4096);
    let abs = workspace().join(&rel);
    let ino = {
        let fs = mount(&abs, false);
        let ino = create(&fs, "file");
        fs.unmount().unwrap();
        ino
    };
    let disk = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&abs)
        .unwrap();
    disk.set_len((BOUNDARY + 32768) * 4096).unwrap();
    // Fake bitmaps live in unused blocks near the end of the real image.
    let (low_bitmap, high_bitmap) = (16000u64, 16001u64);
    let mut bm = vec![0xFFu8; 4096];
    for &bit in free_low {
        bm[bit / 8] &= !(1 << (bit % 8));
    }
    disk.write_all_at(&bm, low_bitmap * 4096).unwrap();
    disk.write_all_at(&[0; 4096], high_bitmap * 4096).unwrap();

    let mut v = Volume::open(
        Box::new(FileDevice::open(&abs, true).unwrap()),
        &MountOptions::default(),
    )
    .unwrap();
    v.sb.set_blocks_count(BOUNDARY + 32768);
    v.sb.set_feature_ro_compat(
        v.sb.feature_ro_compat() & !(ro_compat::METADATA_CSUM | ro_compat::GDT_CSUM),
    );
    let mut inode = v.read_inode(ino).unwrap();
    inode.set_flags(0);
    inode.i_block_mut().fill(0);
    inode.i_block_mut()[11 * 4..12 * 4].copy_from_slice(&last_ptr.to_le_bytes());
    inode.set_size(12 * 4096);
    v.write_inode(ino, &inode).unwrap();
    v.groups.resize(HIGH_GROUP + 1, GroupDesc::new(64));
    for g in &mut v.groups {
        g.set_free_blocks_count(0);
    }
    v.groups[LOW_GROUP].set_block_bitmap(low_bitmap);
    v.groups[LOW_GROUP].set_free_blocks_count(free_low.len() as u32);
    v.groups[HIGH_GROUP].set_block_bitmap(high_bitmap);
    v.groups[HIGH_GROUP].set_free_blocks_count(32768);
    v.sb.set_free_blocks_count(32768 + free_low.len() as u64);
    (v, ino, rel)
}

/// Write block 12 (the first one behind the single indirect block) and
/// return (indirect block, data block).
fn write_behind_indirect(v: &mut Volume, ino: u32) -> ext4_core::Result<(u64, u64)> {
    v.write(ino, 12 * 4096, &[0x99; 4096])?;
    let inode = v.read_inode(ino).unwrap();
    let indirect = u32::from_le_bytes(inode.i_block()[48..52].try_into().unwrap()) as u64;
    let data = u32::from_le_bytes(v.read_block(indirect).unwrap()[..4].try_into().unwrap()) as u64;
    let mut back = [0u8; 4096];
    v.read_file(ino, &inode, 12 * 4096, &mut back).unwrap();
    assert_eq!(back, [0x99; 4096], "readback through the block map");
    Ok((indirect, data))
}

fn drop_boundary_volume(v: Volume, rel: &str) {
    // The fake geometry must never be committed; just let it go.
    std::mem::forget(v);
    let _ = fs::remove_file(workspace().join(rel));
}

#[test]
fn data_block_map_indirect_blocks_stay_below_2_32() {
    // The data block takes the last low block; the indirect block must come
    // from the remaining low block, not from the free group above 2^32.
    let (mut v, ino, rel) = boundary_volume("data-high-indirect", &[32760, 32767], u32::MAX - 1);
    let (indirect, data) = write_behind_indirect(&mut v, ino).unwrap();
    assert_eq!(data, BOUNDARY - 1);
    assert_eq!(indirect, BOUNDARY - 8);
    drop_boundary_volume(v, &rel);
}

#[test]
fn data_block_map_goal_above_2_32_uses_low_blocks() {
    // The allocation goal after block 2^32 - 1 is above the limit, yet low
    // blocks are free: the write must succeed with low blocks.
    let (mut v, ino, rel) = boundary_volume("data-high-goal", &[32760, 32761], u32::MAX);
    let (indirect, data) = write_behind_indirect(&mut v, ino).unwrap();
    assert!(data < BOUNDARY && indirect < BOUNDARY);
    assert_eq!(
        [data, indirect],
        [BOUNDARY - 8, BOUNDARY - 7],
        "data then indirect block from the free low blocks"
    );
    drop_boundary_volume(v, &rel);
}

#[test]
fn data_block_map_without_low_blocks_is_no_space() {
    let (mut v, ino, rel) = boundary_volume("data-high-full", &[32767], u32::MAX - 1);
    assert!(matches!(
        write_behind_indirect(&mut v, ino),
        Err(Error::NoSpace)
    ));
    drop_boundary_volume(v, &rel);
}

/// Without `huge_file`, `i_blocks` is a 32-bit count of 512-byte sectors.
#[test]
fn data_block_count_respects_huge_file() {
    let rel = engine_image("data-ext3-accounting", 64 << 20, Revision::Ext3, 4096);
    let fs = mount(&workspace().join(&rel), false);
    fs.with_volume(|v| {
        assert!(!v.sb.has_ro_compat(ro_compat::HUGE_FILE));
        let mut inode = Inode::new(256);
        assert!(matches!(
            inode.set_blocks_512(&v.sb, 1 << 32),
            Err(Error::FileTooLarge)
        ));
        inode.set_blocks_512(&v.sb, u32::MAX as u64).unwrap();
        assert_eq!(inode.blocks_512(&v.sb), u32::MAX as u64);
        // i_blocks_hi is not part of the count without huge_file.
        inode.set_blocks_512(&v.sb, 8).unwrap();
        inode.set_blocks_hi(1);
        assert_eq!(inode.blocks_512(&v.sb), 8);
        assert_eq!(inode.allocated_bytes(&v.sb), 4096);
    });

    // Linux ext4_max_bitmap_size(12, no huge_file): the data blocks and
    // their indirect blocks must fit a 32-bit sector count.
    const MAX: u64 = 2_196_873_666_560;
    let ino = create(&fs, "big");
    fs.with_volume(|v| {
        let inode = v.read_inode(ino).unwrap();
        assert_eq!(v.max_file_size(&inode), MAX);
    });
    assert!(matches!(fs.write(ino, MAX, &[1]), Err(Error::FileTooLarge)));
    assert_eq!(fs.write(ino, MAX - 1, &[1]).unwrap(), 1);
    assert_eq!(fs.getattr(ino).unwrap().size, MAX);
    assert!(matches!(
        fs.setattr(
            ino,
            &SetAttr {
                size: Some(MAX + 1),
                ..Default::default()
            }
        ),
        Err(Error::FileTooLarge)
    ));
    fs.unmount().unwrap();
    let _ = fs::remove_file(workspace().join(&rel));
}

#[test]
fn data_extent_size_limit_respects_huge_file() {
    let rel = engine_image("data-ext4-limit", 64 << 20, Revision::Ext4, 4096);
    let fs = mount(&workspace().join(&rel), false);
    let ino = create(&fs, "file");
    fs.with_volume(|v| {
        let inode = v.read_inode(ino).unwrap();
        assert!(v.sb.has_ro_compat(ro_compat::HUGE_FILE));
        assert_eq!(v.max_file_size(&inode), (u32::MAX as u64) * 4096);
    });
    fs.unmount().unwrap();
    // The same volume without huge_file: Linux ext4_max_size(12, 0).
    let mut v = Volume::open(
        Box::new(FileDevice::open(workspace().join(&rel), false).unwrap()),
        &MountOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    v.sb.set_feature_ro_compat(v.sb.feature_ro_compat() & !ro_compat::HUGE_FILE);
    let inode = v.read_inode(ino).unwrap();
    assert_eq!(v.max_file_size(&inode), ((u32::MAX as u64) >> 3) << 12);
    drop(v);
    let _ = fs::remove_file(workspace().join(&rel));
}

/// Counts device writes.
struct Counting {
    inner: FileDevice,
    writes: Arc<AtomicU64>,
}

impl BlockDevice for Counting {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> io::Result<()> {
        self.inner.flush()
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
}

fn counting_mount(abs: &Path) -> (Filesystem, Arc<AtomicU64>) {
    let writes = Arc::new(AtomicU64::new(0));
    let dev = Counting {
        inner: FileDevice::open(abs, true).unwrap(),
        writes: writes.clone(),
    };
    let fs = Filesystem::open(Box::new(dev), &MountOptions::default()).unwrap();
    (fs, writes)
}

fn extent_count(fs: &Filesystem, ino: u32) -> usize {
    fs.with_volume(|v| {
        let inode = v.read_inode(ino).unwrap();
        v.read_extents(&inode, ino).unwrap().0.len()
    })
}

/// Device writes to append one block to each of two fragmented files and
/// commit.
fn append_pair_cost(
    fs: &Filesystem,
    writes: &AtomicU64,
    files: [u32; 2],
    blocks: &mut u64,
    bs: u64,
) -> u64 {
    fs.sync().unwrap();
    let before = writes.load(Ordering::Relaxed);
    for ino in files {
        fs.write(ino, *blocks * bs, &block_pattern(ino, *blocks, bs))
            .unwrap();
    }
    *blocks += 1;
    fs.sync().unwrap();
    writes.load(Ordering::Relaxed) - before
}

fn block_pattern(ino: u32, block: u64, bs: u64) -> Vec<u8> {
    pattern(bs as usize, (ino as u64) << 32 | block)
}

/// Alternating appends build two deep extent trees through the
/// path-local append paths (new leaves, full roots, full index nodes).
/// The trees must match e2fsck's expectations exactly, and the metadata
/// written per append must not grow with the number of extents.
#[test]
fn data_extent_appends_edit_only_the_tree_path() {
    if !oracle_or_skip("data_extent_appends_edit_only_the_tree_path") {
        return;
    }
    const BS: u64 = 1024;
    // A 1 KiB leaf holds 84 extents, so 7,200 extents per file fill the
    // first index node (84 leaves) and force a third tree level.
    const PAIRS: u64 = 7_200;
    let (abs, rel) = mke2fs(
        "data-extent-append",
        "64M",
        &["-t", "ext4", "-b", "1024", "-I", "256"],
    );
    let (fs, writes) = counting_mount(&abs);
    let files = [create(&fs, "a"), create(&fs, "b")];
    let mut blocks = 0u64;
    let mut small_cost = 0;
    while blocks < PAIRS {
        if blocks == 500 {
            small_cost = append_pair_cost(&fs, &writes, files, &mut blocks, BS);
            continue;
        }
        for ino in files {
            fs.write(ino, blocks * BS, &block_pattern(ino, blocks, BS))
                .unwrap();
        }
        blocks += 1;
    }
    for ino in files {
        assert_eq!(extent_count(&fs, ino), PAIRS as usize);
    }
    let large_cost = append_pair_cost(&fs, &writes, files, &mut blocks, BS);
    assert!(
        large_cost <= small_cost + 4,
        "append cost grew with the tree: {small_cost} writes at 500 extents, \
         {large_cost} at {PAIRS}"
    );

    // Contiguous appends to one file grow its last extent in place.  The
    // first write starts a new extent; the rest continue it.
    let a = files[0];
    let before = extent_count(&fs, a);
    let first: Vec<u8> = (blocks..blocks + 10)
        .flat_map(|b| block_pattern(a, b, BS))
        .collect();
    fs.write(a, blocks * BS, &first).unwrap();
    blocks += 10;
    for _ in 0..40 {
        fs.write(a, blocks * BS, &block_pattern(a, blocks, BS))
            .unwrap();
        blocks += 1;
    }
    assert_eq!(extent_count(&fs, a), before + 1);
    let b_blocks = blocks - 50;

    // Overwrites of mapped blocks change data only.
    for block in (0..b_blocks).step_by(997) {
        fs.write(a, block * BS + 100, &[0xEE; 300]).unwrap();
    }
    assert_eq!(extent_count(&fs, a), before + 1);

    fs.unmount().unwrap();
    assert_fsck_silent(&rel);
    let fs = mount(&abs, true);
    for (ino, count) in [(files[0], blocks), (files[1], b_blocks)] {
        let data = read_all(&fs, ino);
        assert_eq!(data.len() as u64, count * BS);
        for block in 0..count {
            let mut expect = block_pattern(ino, block, BS);
            if ino == a && block < b_blocks && block % 997 == 0 {
                expect[100..400].fill(0xEE);
            }
            let at = (block * BS) as usize;
            assert_eq!(
                &data[at..at + BS as usize],
                &expect[..],
                "inode {ino} block {block}"
            );
        }
    }
}
