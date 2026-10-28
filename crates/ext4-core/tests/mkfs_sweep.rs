//! Formatter size sweep against `mke2fs -n`.
//!
//! The formatter must use the whole device, as mke2fs does, and only drop a
//! final partial group that is too small to be useful.  Every image must
//! pass `e2fsck -fn` without a single reported problem.

mod support;

use std::fs::{self, File};

use ext4_core::mkfs::{format, MkfsOptions, Revision};
use ext4_core::FileDevice;
use support::*;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

struct Case {
    name: &'static str,
    bytes: u64,
    revision: Revision,
    block_size: u32,
    inode_ratio: u32,
}

/// Format `case` with the engine and return (blocks, groups).
fn engine_format(case: &Case) -> (String, u64) {
    let rel = format!("target/img/{}.img", case.name);
    let abs = workspace().join(&rel);
    fs::create_dir_all(abs.parent().unwrap()).unwrap();
    let _ = fs::remove_file(&abs);
    File::create(&abs).unwrap().set_len(case.bytes).unwrap();
    let dev = FileDevice::open(&abs, true).unwrap();
    let report = format(
        &dev,
        &MkfsOptions {
            revision: case.revision,
            block_size: Some(case.block_size),
            inode_size: Some(256),
            inode_ratio: Some(case.inode_ratio),
            ..MkfsOptions::default()
        },
        &mut |_| {},
    )
    .unwrap_or_else(|e| panic!("{}: format failed: {e}", case.name));
    (rel, report.blocks)
}

/// Block count chosen by `mke2fs -n` for the same device and options.
/// resize_inode is turned off because the engine does not create it, and
/// its reserved descriptor blocks change the final group trimming rule.
fn mke2fs_blocks(case: &Case) -> u64 {
    let rel = format!("target/img/{}-oracle.img", case.name);
    let abs = workspace().join(&rel);
    let _ = fs::remove_file(&abs);
    File::create(&abs).unwrap().set_len(case.bytes).unwrap();
    let kind = match case.revision {
        Revision::Ext2 => "ext2",
        Revision::Ext3 => "ext3",
        Revision::Ext4 => "ext4",
    };
    let block_size = case.block_size.to_string();
    let ratio = case.inode_ratio.to_string();
    let out = e2fs(&[
        "mke2fs",
        "-n",
        "-F",
        "-t",
        kind,
        "-b",
        &block_size,
        "-I",
        "256",
        "-i",
        &ratio,
        "-O",
        "^resize_inode",
        &rel,
    ]);
    let _ = fs::remove_file(&abs);
    let text = text(&out);
    assert!(out.status.success(), "mke2fs -n failed:\n{text}");
    text.split("Creating filesystem with ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("cannot parse mke2fs -n output:\n{text}"))
}

/// Assert `e2fsck -fn` ran every pass and printed nothing else.  A zero
/// exit status alone is not enough: some problems answered "no" still
/// exit cleanly.
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

fn sweep(cases: &[Case]) {
    for case in cases {
        let (rel, blocks) = engine_format(case);
        assert_eq!(
            blocks,
            mke2fs_blocks(case),
            "{}: block count differs from mke2fs -n",
            case.name
        );
        let dump = debugfs_r(&rel, "stats");
        assert!(
            dump.contains(&format!("Block count:              {blocks}")),
            "{}: superblock block count:\n{dump}",
            case.name
        );
        assert_fsck_silent(&rel);

        // Allocate through the engine across the packed metadata area and
        // check again.
        let abs = workspace().join(&rel);
        let fs = mount(&abs, false);
        let file = fs
            .create(2, b"payload", 0o100644, 0, 0, (0, 0))
            .unwrap()
            .ino;
        let data = pattern(3 * MIB as usize + 123, blocks);
        assert_eq!(fs.write(file, 0, &data).unwrap(), data.len());
        fs.unmount().unwrap();
        assert_fsck_silent(&rel);
        let fs = mount(&abs, true);
        assert_eq!(read_all(&fs, file), data, "{}: readback", case.name);
        drop(fs);
        let _ = fs::remove_file(&abs);
    }
}

fn ext4(name: &'static str, bytes: u64, block_size: u32, inode_ratio: u32) -> Case {
    Case {
        name,
        bytes,
        revision: Revision::Ext4,
        block_size,
        inode_ratio,
    }
}

#[test]
fn mkfs_sweep_one_kib_blocks_match_mke2fs() {
    if !oracle_or_skip("mkfs_sweep_one_kib_blocks_match_mke2fs") {
        return;
    }
    sweep(&[
        ext4("mkfs-sweep-8m-1k", 8 * MIB, 1024, 4096),
        ext4("mkfs-sweep-16m-1k", 16 * MIB, 1024, 4096),
        ext4("mkfs-sweep-64m-1k", 64 * MIB, 1024, 4096),
        ext4("mkfs-sweep-100m-1k", 100 * MIB, 1024, 4096),
        ext4("mkfs-sweep-256m-1k", 256 * MIB, 1024, 4096),
        ext4("mkfs-sweep-300m-1k", 300 * MIB, 1024, 4096),
        ext4("mkfs-sweep-1g-1k", GIB, 1024, 16384),
        ext4("mkfs-sweep-4g-1k", 4 * GIB, 1024, 16384),
        // A final group of 100 blocks is too small and is dropped; one of
        // 1023 blocks is kept.
        ext4("mkfs-sweep-trim-1k", 64 * MIB + 101 * KIB, 1024, 4096),
        ext4("mkfs-sweep-keep-1k", 65 * MIB, 1024, 4096),
        Case {
            name: "mkfs-sweep-ext2-100m-1k",
            bytes: 100 * MIB,
            revision: Revision::Ext2,
            block_size: 1024,
            inode_ratio: 4096,
        },
        Case {
            name: "mkfs-sweep-ext3-trim-1k",
            bytes: 64 * MIB + 101 * KIB,
            revision: Revision::Ext3,
            block_size: 1024,
            inode_ratio: 4096,
        },
    ]);
}

#[test]
fn mkfs_sweep_four_kib_blocks_match_mke2fs() {
    if !oracle_or_skip("mkfs_sweep_four_kib_blocks_match_mke2fs") {
        return;
    }
    sweep(&[
        ext4("mkfs-sweep-16m-4k", 16 * MIB, 4096, 4096),
        ext4("mkfs-sweep-64m-4k", 64 * MIB, 4096, 4096),
        ext4("mkfs-sweep-100m-4k", 100 * MIB, 4096, 4096),
        ext4("mkfs-sweep-256m-4k", 256 * MIB, 4096, 4096),
        ext4("mkfs-sweep-300m-4k", 300 * MIB, 4096, 16384),
        ext4("mkfs-sweep-1g-4k", GIB, 4096, 16384),
        ext4("mkfs-sweep-4g-4k", 4 * GIB, 4096, 4096),
        ext4("mkfs-sweep-4g-4k-default", 4 * GIB, 4096, 16384),
        Case {
            name: "mkfs-sweep-ext3-300m-4k",
            bytes: 300 * MIB,
            revision: Revision::Ext3,
            block_size: 4096,
            inode_ratio: 16384,
        },
    ]);
}

/// The reviewer's two reports: a 256 MiB default format and a 4 GiB format
/// with 4 KiB blocks and one inode per 4 KiB.
#[test]
fn mkfs_sweep_reported_sizes_keep_full_capacity() {
    if !oracle_or_skip("mkfs_sweep_reported_sizes_keep_full_capacity") {
        return;
    }
    for (name, bytes, opts, expected) in [
        (
            "mkfs-reported-256m",
            256 * MIB,
            MkfsOptions::default(),
            262_144u64,
        ),
        (
            "mkfs-reported-4g",
            4 * GIB,
            MkfsOptions {
                block_size: Some(4096),
                inode_ratio: Some(4096),
                ..MkfsOptions::default()
            },
            1_048_576,
        ),
        // A 1 GiB, 4 KiB format has a full last group holding only a
        // superblock backup.  Its block bitmap must still be initialized.
        (
            "mkfs-reported-1g-last",
            GIB,
            MkfsOptions {
                block_size: Some(4096),
                ..MkfsOptions::default()
            },
            262_144,
        ),
    ] {
        let rel = format!("target/img/{name}.img");
        let abs = workspace().join(&rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        let _ = fs::remove_file(&abs);
        File::create(&abs).unwrap().set_len(bytes).unwrap();
        let dev = FileDevice::open(&abs, true).unwrap();
        let report = format(&dev, &opts, &mut |_| {}).unwrap();
        drop(dev);
        assert_eq!(report.blocks, expected, "{name}");
        assert_fsck_silent(&rel);
        let _ = fs::remove_file(&abs);
    }
}
