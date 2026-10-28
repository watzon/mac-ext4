//! Journal crash interoperability against e2fsprogs and the engine's replay.

mod support;

use ext4_core::Error;
use support::*;

fn copy_image(source: &std::path::Path, name: &str) -> (std::path::PathBuf, String) {
    let rel = format!("target/img/{name}.img");
    let abs = workspace().join(&rel);
    let _ = std::fs::remove_file(&abs);
    std::fs::copy(source, &abs).unwrap();
    (abs, rel)
}

fn assert_e2fsck_repairs(rel: &str) {
    let output = e2fs(&["e2fsck", "-fy", rel]);
    assert!(
        output.status.code().is_some_and(|code| code <= 1),
        "e2fsck -fy {rel}:\n{}",
        text(&output)
    );
    assert_fsck_clean(rel);
}

fn debugfs_has_inode(rel: &str, path: &str) -> bool {
    debugfs_r(rel, &format!("stat {path}")).contains("Inode:")
}

#[test]
fn committed_uncheckpointed_transaction_replays_in_e2fsck_and_the_engine() {
    if !oracle_or_skip("committed_uncheckpointed_transaction_replays_in_e2fsck_and_the_engine") {
        return;
    }
    let (abs, _rel) = mke2fs(
        "interop-journal-commit-crash",
        "64M",
        &["-t", "ext4", "-b", "4096"],
    );
    let stable = pattern(8192, 0x11);
    let after = pattern(12_345, 0x22);
    let fs = mount(&abs, false);
    let stable_ino = fs.create(2, b"stable", 0o100644, 0, 0, (0, 0)).unwrap().ino;
    fs.write(stable_ino, 0, &stable).unwrap();
    fs.sync().unwrap();

    let after_ino = fs
        .create(2, b"after-crash", 0o100644, 0, 0, (0, 0))
        .unwrap()
        .ino;
    fs.write(after_ino, 0, &after).unwrap();
    fs.mkdir(2, b"after-crash-dir", 0o755, 0, 0).unwrap();
    fs.crash_after_commit_block().unwrap();
    drop(fs);

    let (fsck_abs, fsck_rel) = copy_image(&abs, "interop-journal-commit-crash-fsck");
    assert_e2fsck_repairs(&fsck_rel);
    let stat = debugfs_r(&fsck_rel, "stat after-crash");
    assert!(
        stat.contains("Inode:"),
        "debugfs did not find post-crash file:\n{stat}"
    );
    assert!(
        stat.contains("Size: 12345"),
        "debugfs stat after-crash:\n{stat}"
    );
    assert!(debugfs_has_inode(&fsck_rel, "after-crash-dir"));
    let fs = mount(&fsck_abs, false);
    assert_eq!(read_all(&fs, resolve(&fs, "stable")), stable);
    let replayed_by_fsck = read_all(&fs, resolve(&fs, "after-crash"));
    assert_eq!(replayed_by_fsck, after);
    fs.unmount().unwrap();
    assert_fsck_clean(&fsck_rel);

    let (engine_abs, engine_rel) = copy_image(&abs, "interop-journal-commit-crash-engine");
    let fs = mount(&engine_abs, false);
    let report = fs.recovery_report();
    assert!(
        report.journal_transactions_replayed > 0,
        "engine did not replay the committed journal transaction: {report:?}"
    );
    assert!(
        report.journal_blocks_replayed > 0,
        "engine replay did not apply metadata blocks: {report:?}"
    );
    assert_eq!(read_all(&fs, resolve(&fs, "stable")), stable);
    assert_eq!(read_all(&fs, resolve(&fs, "after-crash")), replayed_by_fsck);
    assert!(fs.lookup(2, b"after-crash-dir").is_ok());
    fs.unmount().unwrap();
    assert_fsck_clean(&engine_rel);
}

#[test]
fn uncommitted_mutations_are_invisible_after_a_crash() {
    if !oracle_or_skip("uncommitted_mutations_are_invisible_after_a_crash") {
        return;
    }
    let (abs, _rel) = mke2fs(
        "interop-journal-uncommitted-crash",
        "64M",
        &["-t", "ext4", "-b", "4096"],
    );
    let stable = pattern(4097, 0x33);
    let fs = mount(&abs, false);
    let stable_ino = fs.create(2, b"stable", 0o100644, 0, 0, (0, 0)).unwrap().ino;
    fs.write(stable_ino, 0, &stable).unwrap();
    fs.sync().unwrap();
    let volatile = fs
        .create(2, b"volatile", 0o100644, 0, 0, (0, 0))
        .unwrap()
        .ino;
    fs.write(volatile, 0, &pattern(32_768, 0x44)).unwrap();
    std::mem::forget(fs);

    let (fsck_abs, fsck_rel) = copy_image(&abs, "interop-journal-uncommitted-crash-fsck");
    assert_e2fsck_repairs(&fsck_rel);
    assert!(debugfs_has_inode(&fsck_rel, "stable"));
    assert!(!debugfs_has_inode(&fsck_rel, "volatile"));
    let fs = mount(&fsck_abs, false);
    assert_eq!(read_all(&fs, resolve(&fs, "stable")), stable);
    assert!(matches!(fs.lookup(2, b"volatile"), Err(Error::NotFound)));
    fs.unmount().unwrap();
    assert_fsck_clean(&fsck_rel);

    let (engine_abs, engine_rel) = copy_image(&abs, "interop-journal-uncommitted-crash-engine");
    let fs = mount(&engine_abs, false);
    assert_eq!(read_all(&fs, resolve(&fs, "stable")), stable);
    assert!(matches!(fs.lookup(2, b"volatile"), Err(Error::NotFound)));
    fs.unmount().unwrap();
    assert_fsck_clean(&engine_rel);
}
