//! Commit, recovery, and durability behavior of a mounted volume.
mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ext4_core::inode::{FL_DIRSYNC, FL_SYNC};
use ext4_core::mkfs::{format, MkfsOptions};
use ext4_core::volume::Volume;
use ext4_core::{BlockDevice, Error, Filesystem, MemDevice, MountOptions};

/// A memory device that counts writes and flushes and can fail one read.
struct TestDevice {
    inner: MemDevice,
    fail_read_at: AtomicU64,
    writes: AtomicU64,
    flushes: AtomicU64,
}

impl TestDevice {
    fn formatted(size: usize, block_size: u32) -> Arc<TestDevice> {
        let dev = Arc::new(TestDevice {
            inner: MemDevice::new(size),
            fail_read_at: AtomicU64::new(u64::MAX),
            writes: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
        });
        format(
            &*dev,
            &MkfsOptions {
                block_size: Some(block_size),
                ..Default::default()
            },
            &mut |_| {},
        )
        .unwrap();
        dev
    }

    fn reset_counts(&self) {
        self.writes.store(0, Ordering::SeqCst);
        self.flushes.store(0, Ordering::SeqCst);
    }

    fn flushes(&self) -> u64 {
        self.flushes.load(Ordering::SeqCst)
    }

    /// Copy of the device as it is now, like the disk after a power cut.
    fn crash_copy(&self) -> MemDevice {
        MemDevice::from_vec(self.inner.snapshot())
    }
}

impl BlockDevice for TestDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if self
            .fail_read_at
            .compare_exchange(offset, u64::MAX, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(std::io::Error::other("injected one-shot read fault"));
        }
        self.inner.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> std::io::Result<()> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
}

fn mount(dev: Box<dyn BlockDevice>) -> Filesystem {
    Filesystem::open(dev, &MountOptions::default()).unwrap()
}

/// Run `e2fsck -fn` on `dev` and require a clean report: exit status 0 and
/// no output beyond the pass headers and the summary.
fn assert_fsck_clean(name: &str, dev: &MemDevice) {
    if !support::oracle_or_skip(name) {
        return;
    }
    let rel = format!("target/img/{name}.img");
    let abs = support::workspace().join(&rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(&abs, dev.snapshot()).unwrap();
    let out = support::e2fs(&["e2fsck", "-fn", &rel]);
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

#[test]
fn staging_read_failure_keeps_the_running_transaction_whole() {
    let dev = TestDevice::formatted(64 << 20, 4096);
    let mut v = Volume::open(Box::new(dev.clone()), &MountOptions::default()).unwrap();
    let ino = v.create(2, b"created", 0o100644, 0, 0, (0, 0)).unwrap();
    v.write(ino, 0, &[0x42; 4096]).unwrap();
    // Read every block so the primary group descriptor block leaves the
    // metadata cache and commit has to read it again.
    for b in 0..(64 << 20) / 4096 {
        v.read_block(b).unwrap();
    }
    dev.fail_read_at.store(4096, Ordering::SeqCst);
    let first = v.commit();
    assert!(
        matches!(first, Err(Error::Io(_))),
        "the injected fault must hit commit staging: {first:?}"
    );
    assert!(v.has_pending(), "the running transaction was dropped");
    v.commit().unwrap();
    v.unmount().unwrap();
    drop(v);

    let fs = mount(Box::new(dev.clone()));
    fs.create(2, b"next", 0o100644, 0, 0, (0, 0)).unwrap();
    fs.unmount().unwrap();
    assert_fsck_clean("journal-volume-staging-fault", &dev.inner);
}

#[test]
fn sync_does_not_flush_twice_after_a_commit() {
    let dev = TestDevice::formatted(16 << 20, 4096);
    let fs = mount(Box::new(dev.clone()));
    fs.create(2, b"f", 0o100644, 0, 0, (0, 0)).unwrap();
    dev.reset_counts();
    fs.sync().unwrap();
    // The commit ends with a flush; sync adds none.
    assert_eq!(dev.flushes(), 4);
    dev.reset_counts();
    fs.sync().unwrap();
    // Nothing pending: one flush for data written in place.
    assert_eq!(dev.flushes(), 1);
    fs.unmount().unwrap();
}

/// The writer never logs revoke records. That is safe only because a
/// commit checkpoints its blocks and empties the log before returning, so a
/// crash can leave at most the newest transaction to replay and no older
/// copy of a block that was freed and reused since.
#[test]
fn the_log_never_holds_an_older_transaction() {
    let dev = TestDevice::formatted(16 << 20, 4096);
    let fs = mount(Box::new(dev.clone()));
    let dir = fs.mkdir(2, b"d", 0o755, 0, 0).unwrap().ino;
    for i in 0..64 {
        fs.create(dir, format!("f{i}").as_bytes(), 0o100644, 0, 0, (0, 0))
            .unwrap();
    }
    fs.sync().unwrap();
    for i in 0..64 {
        fs.remove(dir, format!("f{i}").as_bytes()).unwrap();
    }
    fs.remove(2, b"d").unwrap();
    fs.sync().unwrap();
    let file = fs.create(2, b"new", 0o100644, 0, 0, (0, 0)).unwrap().ino;
    fs.write(file, 0, &[0x77; 8192]).unwrap();
    fs.crash_after_commit_block().unwrap();
    let crashed = dev.crash_copy();
    std::mem::forget(fs);

    let fs = Filesystem::open(
        Box::new(crashed),
        &MountOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(fs.recovery_report().journal_transactions_replayed, 1);
    let mut buf = [0u8; 8192];
    assert_eq!(fs.read(file, 0, &mut buf).unwrap(), 8192);
    assert!(buf.iter().all(|&b| b == 0x77));
}

/// Add `flags` to inode `ino` with a direct inode edit, as Linux `chattr`
/// would.
fn set_flags(dev: &Arc<TestDevice>, ino: u32, flags: u32) {
    let mut v = Volume::open(Box::new(dev.clone()), &MountOptions::default()).unwrap();
    let mut inode = v.read_inode(ino).unwrap();
    inode.set_flags(inode.flags() | flags);
    v.write_inode(ino, &inode).unwrap();
    v.unmount().unwrap();
}

#[test]
fn sync_flagged_inode_writes_are_durable_on_return() {
    let dev = TestDevice::formatted(16 << 20, 4096);
    let (synced, plain) = {
        let fs = mount(Box::new(dev.clone()));
        let synced = fs.create(2, b"synced", 0o100644, 0, 0, (0, 0)).unwrap().ino;
        let plain = fs.create(2, b"plain", 0o100644, 0, 0, (0, 0)).unwrap().ino;
        fs.unmount().unwrap();
        (synced, plain)
    };
    set_flags(&dev, synced, FL_SYNC);

    let fs = mount(Box::new(dev.clone()));
    dev.reset_counts();
    assert_eq!(fs.write(plain, 0, b"lazy").unwrap(), 4);
    assert_eq!(dev.flushes(), 0, "an ordinary write must not flush");
    assert_eq!(fs.write(synced, 0, b"durable").unwrap(), 7);
    assert!(dev.flushes() > 0, "FL_SYNC write returned without a flush");

    // Power cut right after the write returned.
    let crashed = dev.crash_copy();
    std::mem::forget(fs);
    let fs = mount(Box::new(crashed));
    assert_eq!(fs.getattr(synced).unwrap().size, 7);
    let mut buf = [0u8; 7];
    fs.read(synced, 0, &mut buf).unwrap();
    assert_eq!(&buf, b"durable");
    fs.unmount().unwrap();
}

#[test]
fn dirsync_directory_entry_changes_are_durable_on_return() {
    let dev = TestDevice::formatted(16 << 20, 4096);
    let dir = {
        let fs = mount(Box::new(dev.clone()));
        let dir = fs.mkdir(2, b"d", 0o755, 0, 0).unwrap().ino;
        fs.unmount().unwrap();
        dir
    };
    set_flags(&dev, dir, FL_DIRSYNC);

    let fs = mount(Box::new(dev.clone()));
    dev.reset_counts();
    fs.create(2, b"outside", 0o100644, 0, 0, (0, 0)).unwrap();
    assert_eq!(dev.flushes(), 0);
    fs.create(dir, b"inside", 0o100644, 0, 0, (0, 0)).unwrap();
    assert!(
        dev.flushes() > 0,
        "FL_DIRSYNC create returned without a flush"
    );

    let crashed = dev.crash_copy();
    std::mem::forget(fs);
    let fs = mount(Box::new(crashed));
    fs.lookup(dir, b"inside").unwrap();
    fs.unmount().unwrap();
}

#[test]
fn large_write_reports_progress_when_a_later_chunk_fails() {
    let dev = TestDevice::formatted(64 << 20, 4096);
    let fs = mount(Box::new(dev.clone()));
    let ino = fs.create(2, b"big", 0o100644, 0, 0, (0, 0)).unwrap().ino;
    let st = fs.statfs();
    let free = st.avail_blocks * u64::from(st.block_size);
    // Leave about 12 MiB free, then write 16 MiB: the first 8 MiB chunk
    // fits, the second does not.
    let filler = fs.create(2, b"filler", 0o100644, 0, 0, (0, 0)).unwrap().ino;
    let fill = vec![0x11u8; (free - (12 << 20)) as usize];
    assert_eq!(fs.write(filler, 0, &fill).unwrap(), fill.len());

    let data = vec![0xA5u8; 16 << 20];
    assert_eq!(fs.write(ino, 0, &data).unwrap(), 8 << 20);
    assert_eq!(fs.getattr(ino).unwrap().size, 8 << 20);

    // No progress at all: the error is returned.
    let tail = vec![0x5Au8; 16 << 20];
    assert!(matches!(
        fs.write(ino, 8 << 20, &tail[..8 << 20]),
        Err(Error::NoSpace)
    ));
    fs.unmount().unwrap();
}

#[test]
fn read_dir_plus_returns_getattr_attributes_under_one_lock() {
    let dev = TestDevice::formatted(16 << 20, 4096);
    let fs = mount(Box::new(dev.clone()));
    let dir = fs.mkdir(2, b"d", 0o755, 0, 0).unwrap().ino;
    let file = fs.create(dir, b"file", 0o100600, 0, 0, (0, 0)).unwrap().ino;
    fs.write(file, 0, b"hello").unwrap();
    fs.mkdir(dir, b"sub", 0o700, 0, 0).unwrap();

    let mut seen = Vec::new();
    fs.read_dir_plus(dir, 0, true, |entry, attr, _| {
        seen.push((entry.name.clone(), entry.inode, attr.cloned()));
        true
    })
    .unwrap();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    let names: Vec<&[u8]> = seen.iter().map(|(n, _, _)| n.as_slice()).collect();
    assert_eq!(names, vec![&b"."[..], b"..", b"file", b"sub"]);
    for (name, ino, attr) in &seen {
        if name == b"." || name == b".." {
            assert!(attr.is_none());
            continue;
        }
        let expected = fs.getattr(*ino).unwrap();
        let attr = attr.as_ref().expect("attributes for a named entry");
        assert_eq!(attr.ino, expected.ino);
        assert_eq!(attr.size, expected.size);
        assert_eq!(attr.mode, expected.mode);
        assert_eq!(attr.nlink, expected.nlink);
        assert_eq!(attr.parent, expected.parent);
        assert_eq!(attr.mtime, expected.mtime);
    }

    let mut count = 0;
    fs.read_dir_plus(dir, 0, false, |_, attr, _| {
        assert!(attr.is_none());
        count += 1;
        true
    })
    .unwrap();
    assert_eq!(count, 4);
    fs.unmount().unwrap();
}

/// A transaction written by Linux tools (`debugfs journal_open -c`) must be
/// replayed, for checksum v2 and v3.
#[test]
fn replays_linux_checksum_v2_and_v3_transactions() {
    if !support::oracle_available("linux journal replay", &["mke2fs", "debugfs", "e2fsck"]) {
        return;
    }
    for version in ["2", "3"] {
        let name = format!("journal-volume-native-v{version}");
        let (abs, rel) = support::mke2fs(&name, "64M", &["-t", "ext4", "-b", "4096"]);
        let payload_rel = format!("target/img/{name}.payload");
        let payload = vec![b'J'; 4096];
        std::fs::write(support::workspace().join(&payload_rel), &payload).unwrap();
        let out = support::debugfs_w(
            &rel,
            &format!(
                "journal_open -c -v {version}\njournal_write -b 10000 {payload_rel}\njournal_close\n"
            ),
        );
        assert!(out.status.success(), "{}", support::text(&out));

        // Read-only mount: the replay is an in-memory overlay.
        let fs = mount_image(&abs, true, version);
        assert_eq!(
            fs.recovery_report().journal_transactions_replayed,
            1,
            "v{version}"
        );
        let block = fs.with_volume(|v| v.read_block(10000).unwrap());
        assert_eq!(&block[..], &payload[..], "v{version}");
        drop(fs);

        // Writable mount: the replay reaches the disk.
        let fs = mount_image(&abs, false, version);
        assert_eq!(
            fs.recovery_report().journal_blocks_replayed,
            1,
            "v{version}"
        );
        fs.unmount().unwrap();
        let mut on_disk = vec![0u8; 4096];
        ext4_core::FileDevice::open(&abs, false)
            .unwrap()
            .read_at(10000 * 4096, &mut on_disk)
            .unwrap();
        assert_eq!(on_disk, payload, "v{version}");
        assert_fsck_clean(&name, &MemDevice::from_vec(std::fs::read(&abs).unwrap()));
    }
}

fn mount_image(abs: &std::path::Path, read_only: bool, version: &str) -> Filesystem {
    let dev = ext4_core::FileDevice::open(abs, !read_only).unwrap();
    Filesystem::open(
        Box::new(dev),
        &MountOptions {
            read_only,
            ..Default::default()
        },
    )
    .unwrap_or_else(|e| panic!("mount v{version} read_only={read_only}: {e:?}"))
}
