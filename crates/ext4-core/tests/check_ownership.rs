//! Checker block ownership: external xattr blocks (shared and unshared, on
//! every inode type) and repair when some owner cannot be read.

mod support;

use ext4_core::check::{check, CheckOptions, CheckReport};
use ext4_core::crc::crc32c;
use ext4_core::volume::read_superblock;
use ext4_core::FileDevice;

use support::*;

fn run(path: &std::path::Path, repair: bool) -> CheckReport {
    check(
        Box::new(FileDevice::open(path, repair).unwrap()),
        &CheckOptions {
            repair,
            quick: false,
            force: true,
        },
        &mut |_| {},
        &mut |_| {},
    )
    .unwrap()
}

/// `e2fsck -fn` exits 0 and prints nothing beyond its pass banners.
fn assert_fsck_no_problems(rel: &str) {
    let o = e2fs(&["e2fsck", "-fn", rel]);
    let out = text(&o);
    assert!(o.status.success(), "e2fsck -fn {rel}:\n{out}");
    for bad in ["Fix?", "WARNING", "differences", "Clear?"] {
        assert!(
            !out.contains(bad),
            "e2fsck -fn {rel} reported {bad}:\n{out}"
        );
    }
}

fn stat_field(rel: &str, file: &str, field: &str) -> u64 {
    let out = debugfs_r(rel, &format!("stat {file}"));
    let at = out
        .find(field)
        .unwrap_or_else(|| panic!("{field} in stat {file}:\n{out}"));
    out[at + field.len()..]
        .split_whitespace()
        .next()
        .unwrap()
        .trim_end_matches(',')
        .parse()
        .unwrap()
}

/// Set `h_refcount` of xattr block `blk` and reseal its checksum.
fn set_xattr_refcount(abs: &std::path::Path, blk: u64, refs: u32) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let sb = read_superblock(&FileDevice::open(abs, false).unwrap()).unwrap();
    let bs = sb.block_size() as u64;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(abs)
        .unwrap();
    let mut b = vec![0u8; bs as usize];
    f.seek(SeekFrom::Start(blk * bs)).unwrap();
    f.read_exact(&mut b).unwrap();
    b[4..8].copy_from_slice(&refs.to_le_bytes());
    let c = crc32c(sb.csum_seed(), &blk.to_le_bytes());
    let c = crc32c(c, &b[..0x10]);
    let c = crc32c(c, &[0; 4]);
    let c = crc32c(c, &b[0x14..]);
    b[0x10..0x14].copy_from_slice(&c.to_le_bytes());
    f.seek(SeekFrom::Start(blk * bs)).unwrap();
    f.write_all(&b).unwrap();
}

#[test]
fn check_repair_keeps_external_xattr_blocks() {
    if !oracle_or_skip("check_repair_keeps_external_xattr_blocks") {
        return;
    }
    let (abs, rel) = mke2fs(
        "check-xattr-owners",
        "64M",
        &["-t", "ext4", "-b", "4096", "-I", "256", "-O", "inline_data"],
    );
    let root = workspace();
    let value = "v".repeat(1000);
    std::fs::write(root.join("target/img/check-xattr-value"), &value).unwrap();
    std::fs::write(root.join("target/img/check-xattr-small"), b"hi").unwrap();
    let mut script = String::new();
    for (make, name) in [
        ("write /etc/hosts plain", "plain"),
        ("write target/img/check-xattr-small inl", "inl"),
        ("symlink fast /x", "fast"),
        ("mknod dev c 1 2", "dev"),
        ("write target/img/check-xattr-small s1", "s1"),
        ("write target/img/check-xattr-small s2", "s2"),
        ("symlink fast2 /y", "fast2"),
    ] {
        script.push_str(&format!(
            "{make}\nea_set -f target/img/check-xattr-value {name} user.big\n"
        ));
    }
    let out = debugfs_w(&rel, &script);
    assert!(out.status.success(), "{}", text(&out));
    assert!(debugfs_r(&rel, "stat inl").contains("Size of inline data"));
    assert!(debugfs_r(&rel, "stat fast").contains("Fast link dest"));

    // Share s1's block among s1, s2, fast2, and dev (h_refcount 4).
    let shared = stat_field(&rel, "s1", "File ACL:");
    let mut cmds = String::new();
    for name in ["s2", "fast2", "dev"] {
        let own = stat_field(&rel, name, "File ACL:");
        cmds.push_str(&format!("sif {name} file_acl {shared}\nfreeb {own}\n"));
    }
    let out = debugfs_w(&rel, &cmds);
    assert!(out.status.success(), "{}", text(&out));
    set_xattr_refcount(&abs, shared, 4);
    // debugfs freeb leaves the free counts alone; let e2fsck settle them so
    // the fixture itself is valid.
    let settle = e2fs(&["e2fsck", "-fy", &rel]);
    assert!(
        settle.status.code().is_some_and(|c| c <= 1),
        "{}",
        text(&settle)
    );
    assert_fsck_no_problems(&rel);
    assert_eq!(stat_field(&rel, "dev", "File ACL:"), shared);

    let report = run(&abs, false);
    assert!(report.clean, "{:?}", report.problems);
    let repaired = run(&abs, true);
    assert!(repaired.clean, "{:?}", repaired.problems);
    assert!(
        !repaired.repaired.iter().any(|r| r.contains("bitmap")),
        "{:?}",
        repaired.repaired
    );
    assert_fsck_no_problems(&rel);
    for name in ["plain", "inl", "fast", "dev", "s1", "s2", "fast2"] {
        let blk = stat_field(&rel, name, "File ACL:");
        assert!(
            debugfs_r(&rel, &format!("testb {blk}")).contains("marked in use"),
            "{name} xattr block {blk}"
        );
    }

    // A wrong reference count is reported.
    set_xattr_refcount(&abs, shared, 3);
    let report = run(&abs, false);
    assert!(
        report.problems.iter().any(|p| p.contains(&format!(
            "xattr block {shared} has reference count 3, expected 4"
        ))),
        "{:?}",
        report.problems
    );
}

#[test]
fn check_repair_does_not_free_blocks_of_unreadable_inode() {
    if !oracle_or_skip("check_repair_does_not_free_blocks_of_unreadable_inode") {
        return;
    }
    let (abs, rel) = mke2fs("check-unreadable-map", "64M", &["-t", "ext4", "-b", "4096"]);
    let out = debugfs_w(&rel, "write /etc/hosts victim");
    assert!(out.status.success(), "{}", text(&out));
    let blocks = debugfs_r(&rel, "blocks victim");
    let victim_block: u64 = blocks
        .split_whitespace()
        .filter_map(|w| w.parse().ok())
        .next()
        .unwrap_or_else(|| panic!("victim blocks: {blocks}"));
    // Break only the extent header magic.
    let out = debugfs_w(&rel, "set_inode_field victim block[0] 0");
    assert!(out.status.success(), "{}", text(&out));
    // Also make the free block count wrong, which repair would normally fix.
    let out = debugfs_w(
        &rel,
        "set_bg 0 free_blocks_count 7\nssv free_blocks_count 9",
    );
    assert!(out.status.success(), "{}", text(&out));

    let repaired = run(&abs, true);
    assert!(!repaired.clean);
    assert!(
        repaired
            .problems
            .iter()
            .any(|p| p.contains("block map") && p.contains("magic")),
        "{:?}",
        repaired.problems
    );
    assert!(
        repaired
            .problems
            .iter()
            .any(|p| p.contains("block ownership could not be fully determined")),
        "{:?}",
        repaired.problems
    );
    assert!(
        !repaired
            .repaired
            .iter()
            .any(|r| r.contains("block bitmap") || r.contains("free blocks")),
        "{:?}",
        repaired.repaired
    );
    assert!(
        debugfs_r(&rel, &format!("testb {victim_block}")).contains("marked in use"),
        "victim block {victim_block} was freed"
    );
}

#[test]
fn check_repair_does_not_lower_links_from_unreadable_directory() {
    if !oracle_or_skip("check_repair_does_not_lower_links_from_unreadable_directory") {
        return;
    }
    let (abs, rel) = mke2fs("check-unreadable-dir", "64M", &["-t", "ext4", "-b", "4096"]);
    let out = debugfs_w(
        &rel,
        "write /etc/hosts f\nmkdir d\nln f d/f2\nsif f links_count 2\nset_inode_field d block[0] 0",
    );
    assert!(out.status.success(), "{}", text(&out));
    let repaired = run(&abs, true);
    assert!(!repaired.clean);
    assert!(
        repaired
            .problems
            .iter()
            .any(|p| p.contains("link counts will not be lowered")),
        "{:?}",
        repaired.problems
    );
    assert!(
        !repaired.repaired.iter().any(|r| r.contains("link count")),
        "{:?}",
        repaired.repaired
    );
    assert!(debugfs_r(&rel, "stat f").contains("Links: 2"));
    assert!(debugfs_r(&rel, "stat /").contains("Links: 4"));
}
