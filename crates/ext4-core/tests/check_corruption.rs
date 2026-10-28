mod support;

use ext4_core::check::{check, CheckOptions};
use ext4_core::FileDevice;

use support::{debugfs_w, mke2fs, oracle_or_skip, text};

fn run(path: &std::path::Path, repair: bool) -> ext4_core::check::CheckReport {
    let dev = Box::new(FileDevice::open(path, true).expect("open image"));
    check(
        dev,
        &CheckOptions {
            repair,
            quick: false,
            force: true,
        },
        &mut |_| {},
        &mut |_| {},
    )
    .expect("check image")
}

#[test]
fn check_reports_and_repairs_block_bitmap_corruption() {
    if !oracle_or_skip("check bitmap corruption") {
        return;
    }
    for (name, command) in [("check-setb", "setb 16000"), ("check-freeb", "freeb 1")] {
        let (path, rel) = mke2fs(name, "64M", &["-t", "ext2", "-b", "1024", "-I", "256"]);
        let out = debugfs_w(&rel, command);
        assert!(out.status.success(), "{}", text(&out));
        let report = run(&path, false);
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("block bitmap differs")),
            "{name}: {:?}",
            report.problems
        );
        let repaired = run(&path, true);
        assert!(repaired.clean, "{name}: {:?}", repaired.problems);
        support::assert_fsck_clean(&rel);
    }
}

#[test]
fn check_reports_directory_reference_to_cleared_inode() {
    if !oracle_or_skip("check cleared inode corruption") {
        return;
    }
    let (path, rel) = mke2fs(
        "check-clri",
        "64M",
        &["-t", "ext4", "-b", "4096", "-I", "256"],
    );
    let out = debugfs_w(&rel, "write /etc/hosts file\nclri <13>");
    assert!(out.status.success(), "{}", text(&out));
    let report = run(&path, false);
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("directory references free inode 13")),
        "{:?}",
        report.problems
    );
}
