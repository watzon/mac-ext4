//! Extent-tree and indexed-directory interoperability coverage.

mod support;

use ext4_core::ops::SetAttr;
use ext4_core::Filesystem;
use support::*;

fn extent_count(fs: &Filesystem, ino: u32) -> usize {
    fs.with_volume(|volume| {
        let inode = volume.read_inode(ino).unwrap();
        volume.read_extents(&inode, ino).unwrap().0.len()
    })
}

fn extent_depth(fs: &Filesystem, ino: u32) -> u16 {
    fs.with_volume(|volume| {
        let inode = volume.read_inode(ino).unwrap();
        let root = inode.i_block();
        u16::from_le_bytes([root[6], root[7]])
    })
}

fn debugfs_blockcount(stat: &str) -> u64 {
    stat.split("Blockcount:")
        .nth(1)
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("debugfs stat did not report Blockcount:\n{stat}"))
}

fn assert_debugfs_extent_file(fs: &Filesystem, rel: &str, path: &str, ino: u32) {
    let stat = debugfs_r(rel, &format!("stat {path}"));
    assert!(stat.contains("EXTENTS:"), "debugfs stat {path}:\n{stat}");
    assert_eq!(
        debugfs_blockcount(&stat),
        fs.getattr(ino).unwrap().alloc_bytes / 512,
        "debugfs and engine disagree about i_blocks for {path}:\n{stat}"
    );
    let dump = debugfs_r(rel, &format!("dump_extents {path}"));
    assert!(
        dump.contains("Level"),
        "debugfs dump_extents {path}:\n{dump}"
    );
}

fn checkpoint(
    fs: Filesystem,
    abs: &std::path::Path,
    rel: &str,
    path: &str,
    ino: u32,
    expected: &[u8],
) -> Filesystem {
    fs.sync().unwrap();
    assert_eq!(read_all(&fs, ino), expected, "engine readback for {path}");
    assert_debugfs_extent_file(&fs, rel, path, ino);
    fs.unmount().unwrap();
    assert_fsck_clean(rel);
    mount(abs, false)
}

fn exercise_extent_tree(block_size: usize, stages: &[usize]) {
    let name = format!("interop-extents-{block_size}");
    let (abs, rel) = mke2fs(
        &name,
        "256M",
        &["-t", "ext4", "-b", &block_size.to_string()],
    );
    let mut fs = mount(&abs, false);
    for &target in stages {
        let sparse_name = format!("sparse-{target}");
        let filler_name = format!("filler-{target}");
        let sparse = fs
            .create(2, sparse_name.as_bytes(), 0o100644, 0, 0, (0, 0))
            .unwrap()
            .ino;
        let filler = fs
            .create(2, filler_name.as_bytes(), 0o100644, 0, 0, (0, 0))
            .unwrap()
            .ino;
        let mut expected = vec![0; (target * 2 - 1) * block_size];

        for index in 0..target {
            let data = pattern(block_size, 0xE000 + index as u64);
            fs.write(sparse, (index * 2 * block_size) as u64, &data)
                .unwrap();
            expected[index * 2 * block_size..(index * 2 + 1) * block_size].copy_from_slice(&data);
            // Consuming the intervening physical block makes the sparse file's
            // logical gaps physical gaps too, so it has one extent per write.
            fs.write(
                filler,
                (index * block_size) as u64,
                &pattern(block_size, 0xF000 + index as u64),
            )
            .unwrap();
        }
        fs = checkpoint(fs, &abs, &rel, &sparse_name, sparse, &expected);
        assert_eq!(
            extent_count(&fs, sparse),
            target,
            "extent count at {target} extents"
        );
        let sparse_depth = extent_depth(&fs, sparse);
        if target >= 5 {
            assert!(
                sparse_depth >= 1,
                "tree did not leave inode root at {target}"
            );
        }
        if block_size == 1024 && target == 8000 {
            assert_eq!(
                sparse_depth, 2,
                "8000 extents must require depth two at 1 KiB"
            );
        }

        fs.setattr(
            filler,
            &SetAttr {
                size: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        fs.sync().unwrap();
        for index in 0..target.saturating_sub(1) {
            let data = pattern(block_size, 0xA000 + index as u64);
            let offset = (index * 2 + 1) * block_size;
            fs.write(sparse, offset as u64, &data).unwrap();
            expected[offset..offset + block_size].copy_from_slice(&data);
        }
        fs = checkpoint(fs, &abs, &rel, &sparse_name, sparse, &expected);
        let merged_count = extent_count(&fs, sparse);
        assert!(
            merged_count < target,
            "filling physical gaps did not merge any of {target} extents"
        );
        assert!(
            extent_depth(&fs, sparse) <= sparse_depth,
            "filling gaps grew the extent tree"
        );

        let full_blocks = target * 2 - 1;
        for blocks in [full_blocks * 3 / 4, full_blocks / 2, 1, 0] {
            let bytes = blocks * block_size;
            fs.setattr(
                sparse,
                &SetAttr {
                    size: Some(bytes as u64),
                    ..Default::default()
                },
            )
            .unwrap();
            expected.truncate(bytes);
            fs = checkpoint(fs, &abs, &rel, &sparse_name, sparse, &expected);
            assert_eq!(fs.getattr(sparse).unwrap().size, bytes as u64);
            if blocks <= 4 {
                assert_eq!(
                    extent_depth(&fs, sparse),
                    0,
                    "small truncated tree must fit in the inode root"
                );
            }
        }
    }

    let first = fs
        .create(2, b"append-a", 0o100644, 0, 0, (0, 0))
        .unwrap()
        .ino;
    let second = fs
        .create(2, b"append-b", 0o100644, 0, 0, (0, 0))
        .unwrap()
        .ino;
    let mut first_expected = Vec::new();
    let mut second_expected = Vec::new();
    for index in 0..400usize {
        let left = pattern(block_size, 0xB000 + index as u64);
        let right = pattern(block_size, 0xC000 + index as u64);
        fs.write(first, (index * block_size) as u64, &left).unwrap();
        fs.write(second, (index * block_size) as u64, &right)
            .unwrap();
        first_expected.extend_from_slice(&left);
        second_expected.extend_from_slice(&right);
    }
    fs.sync().unwrap();
    assert_eq!(read_all(&fs, first), first_expected);
    assert_eq!(read_all(&fs, second), second_expected);
    assert!(
        extent_count(&fs, first) >= 300,
        "interleaved appends merged unexpectedly"
    );
    assert!(
        extent_count(&fs, second) >= 300,
        "interleaved appends merged unexpectedly"
    );
    assert_debugfs_extent_file(&fs, &rel, "append-a", first);
    assert_debugfs_extent_file(&fs, &rel, "append-b", second);
    fs.unmount().unwrap();
    assert_fsck_clean(&rel);
}

#[test]
fn grows_shrinks_and_interleaves_extent_trees_on_linux_images() {
    if !oracle_or_skip("grows_shrinks_and_interleaves_extent_trees_on_linux_images") {
        return;
    }
    exercise_extent_tree(4096, &[3, 5, 400]);
    exercise_extent_tree(1024, &[3, 5, 400, 8000]);
}

#[test]
fn creates_entries_in_linux_indexed_one_kib_directory_without_metadata_checksums() {
    if !oracle_or_skip(
        "creates_entries_in_linux_indexed_one_kib_directory_without_metadata_checksums",
    ) {
        return;
    }
    let (abs, rel) = mke2fs(
        "interop-htree-one-kib-no-csum",
        "128M",
        &["-t", "ext4", "-b", "1024", "-O", "^metadata_csum"],
    );
    let byte_path = workspace().join("target/img/interop-htree-one-kib-no-csum-byte");
    std::fs::write(&byte_path, [0x5A]).unwrap();
    let mut seed = String::from("mkdir indexed\n");
    for index in 0..512 {
        seed.push_str(&format!(
            "write target/img/interop-htree-one-kib-no-csum-byte indexed/seed-{index:04}\n"
        ));
    }
    let output = debugfs_w(&rel, &seed);
    assert!(output.status.success(), "{}", text(&output));
    let indexed = e2fs(&["e2fsck", "-fyD", &rel]);
    assert!(
        indexed.status.code().is_some_and(|code| code <= 1),
        "{}",
        text(&indexed)
    );
    assert!(debugfs_r(&rel, "htree indexed").contains("Hash Version"));

    let fs = mount(&abs, false);
    let dir = fs.lookup(2, b"indexed").unwrap().ino;
    let suffix = "x".repeat(220);
    for index in 0..5_000u32 {
        let name = format!("entry-{index:05}-{suffix}");
        fs.create(dir, name.as_bytes(), 0o100644, 0, 0, (0, 0))
            .unwrap();
    }
    fs.sync().unwrap();
    for index in (0..5_000u32).step_by(53) {
        let name = format!("entry-{index:05}-{suffix}");
        fs.lookup(dir, name.as_bytes()).unwrap();
    }
    fs.unmount().unwrap();
    assert_fsck_clean(&rel);
    assert!(debugfs_r(&rel, "htree indexed").contains("Hash Version"));
}
