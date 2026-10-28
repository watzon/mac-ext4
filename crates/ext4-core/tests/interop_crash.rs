//! Crash-style remount checks for journal and orphan recovery.

mod support;

use ext4_core::Error;
use support::*;

#[test]
fn remount_after_sync_without_unmount_is_fsck_clean() {
    if !oracle_or_skip("remount_after_sync_without_unmount_is_fsck_clean") {
        return;
    }
    let (abs, rel) = mke2fs("interop-crash-sync", "64M", &["-t", "ext4", "-b", "4096"]);
    let content = pattern(64 * 1024, 51);
    let fs = mount(&abs, false);
    let file = fs.create(2, b"persist", 0o100644, 0, 0, (0, 0)).unwrap();
    fs.write(file.ino, 0, &content).unwrap();
    fs.sync().unwrap();
    std::mem::forget(fs);

    let fs = mount(&abs, false);
    assert_eq!(read_all(&fs, resolve(&fs, "persist")), content);
    fs.unmount().unwrap();
    assert_fsck_clean(&rel);
}

#[test]
fn remount_reclaims_persisted_orphans() {
    if !oracle_or_skip("remount_reclaims_persisted_orphans") {
        return;
    }
    let (abs, rel) = mke2fs("interop-crash-orphan", "64M", &["-t", "ext4", "-b", "4096"]);
    let fs = mount(&abs, false);
    let file = fs.create(2, b"orphan", 0o100644, 0, 0, (0, 0)).unwrap();
    fs.write(file.ino, 0, &pattern(9000, 52)).unwrap();
    let orphan = fs.remove(2, b"orphan").unwrap();
    assert_eq!(orphan, file.ino);
    fs.sync().unwrap();
    std::mem::forget(fs);

    let fs = mount(&abs, false);
    assert!(matches!(fs.getattr(orphan), Err(Error::NotFound)));
    fs.unmount().unwrap();
    assert_fsck_clean(&rel);
}
