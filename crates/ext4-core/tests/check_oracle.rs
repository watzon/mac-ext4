mod support;

use ext4_core::check::{check, CheckOptions};
use ext4_core::mkfs::{format, MkfsOptions, Revision};
use ext4_core::{FileDevice, Filesystem, MountOptions};

use support::{assert_fsck_clean, debugfs_w, mke2fs, oracle_or_skip, text, workspace};

fn run_check(path: &std::path::Path, repair: bool) -> ext4_core::check::CheckReport {
    let dev = Box::new(FileDevice::open(path, true).expect("open image"));
    let mut messages = Vec::new();
    let mut progress = Vec::new();
    check(
        dev,
        &CheckOptions {
            repair,
            quick: false,
            force: true,
        },
        &mut |m| messages.push(m.to_owned()),
        &mut |p| progress.push(p),
    )
    .expect("check image")
}

#[test]
fn check_oracle_clean_images() {
    if !oracle_or_skip("check oracle") {
        return;
    }
    for (kind, block) in [
        ("ext2", "1024"),
        ("ext3", "4096"),
        ("ext4", "1024"),
        ("ext4", "4096"),
    ] {
        let (path, rel) = mke2fs(
            &format!("check-clean-{kind}-{block}"),
            "64M",
            &["-t", kind, "-b", block, "-I", "256"],
        );
        let report = run_check(&path, false);
        assert!(report.clean, "{kind}/{block}: {:?}", report.problems);
        assert_fsck_clean(&rel);
    }
}

#[test]
fn check_oracle_our_mkfs_is_clean() {
    if !oracle_or_skip("check mkfs oracle") {
        return;
    }
    let root = workspace();
    let path = root.join("target/img/check-own-mkfs.img");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    std::fs::File::create(&path)
        .expect("image")
        .set_len(64 * 1024 * 1024)
        .expect("size");
    let dev = FileDevice::open(&path, true).expect("open");
    let mut progress = |_| {};
    format(
        &dev,
        &MkfsOptions {
            revision: Revision::Ext4,
            label: "check".into(),
            ..MkfsOptions::default()
        },
        &mut progress,
    )
    .expect("format");
    let report = run_check(&path, false);
    assert!(report.clean, "{:?}", report.problems);
    let filesystem = Filesystem::open(
        Box::new(FileDevice::open(&path, true).expect("reopen")),
        &MountOptions::default(),
    )
    .expect("mount freshly formatted ext4 read-write");
    filesystem.sync().expect("sync freshly formatted ext4");
    filesystem
        .unmount()
        .expect("unmount freshly formatted ext4");
    assert_fsck_clean("target/img/check-own-mkfs.img");
}

#[test]
fn check_oracle_repairs_counts_bitmaps_and_links() {
    if !oracle_or_skip("check repair oracle") {
        return;
    }
    let (path, rel) = mke2fs(
        "check-repair",
        "64M",
        &["-t", "ext4", "-b", "4096", "-I", "256"],
    );
    let out = debugfs_w(&rel, "write /etc/hosts file\nsif <12> links_count 5\nset_bg 0 free_blocks_count 1\nssv free_blocks_count 3");
    assert!(out.status.success(), "{}", text(&out));
    let read_only = run_check(&path, false);
    assert!(!read_only.clean);
    assert!(!read_only.problems.is_empty());
    let repaired = run_check(&path, true);
    assert!(
        repaired.clean,
        "remaining: {:?}; repaired: {:?}",
        repaired.problems, repaired.repaired
    );
    assert_fsck_clean(&rel);
}
