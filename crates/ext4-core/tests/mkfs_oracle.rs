mod support;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

use ext4_core::mkfs::{format, MkfsOptions, Revision};
use ext4_core::{FileDevice, Filesystem, MountOptions};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn oracle_available(test: &str) -> bool {
    support::oracle_or_skip(test)
}

fn e2fs(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("tools/e2fs/run")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run e2fsprogs container")
}

fn format_image(
    root: &Path,
    name: &str,
    bytes: u64,
    revision: Revision,
    block_size: u32,
    inode_size: u16,
) -> PathBuf {
    let rel = format!("target/img/{name}.img");
    let image = root.join(&rel);
    fs::create_dir_all(image.parent().expect("image parent")).expect("create image directory");
    File::create(&image)
        .expect("create image")
        .set_len(bytes)
        .expect("size image");
    let dev = FileDevice::open(&image, true).expect("open image");
    let mut progress = |_| {};
    format(
        &dev,
        &MkfsOptions {
            label: "oracle".into(),
            revision,
            block_size: Some(block_size),
            inode_size: Some(inode_size),
            uuid: Some([0xA5; 16]),
            ..MkfsOptions::default()
        },
        &mut progress,
    )
    .expect("format image");
    image
}

fn assert_usable(root: &Path, rel: &str, revision: Revision, block_size: u32) {
    let filesystem = Filesystem::open(
        Box::new(FileDevice::open(root.join(rel), true).expect("reopen formatted image")),
        &MountOptions::default(),
    )
    .expect("mount formatted image read-write");
    filesystem.sync().expect("sync formatted image");
    filesystem.unmount().expect("unmount formatted image");
    let fsck = e2fs(root, &["e2fsck", "-fn", rel]);
    assert!(
        fsck.status.success(),
        "e2fsck failed:\n{}\n{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );
    let dump = e2fs(root, &["dumpe2fs", "-h", rel]);
    assert!(
        dump.status.success(),
        "dumpe2fs failed: {}",
        String::from_utf8_lossy(&dump.stderr)
    );
    let dump_text = String::from_utf8_lossy(&dump.stdout);
    assert!(
        dump_text.contains("Filesystem volume name:   oracle"),
        "missing label:\n{dump_text}"
    );
    assert!(
        dump_text.contains(&format!("Block size:               {block_size}")),
        "wrong block size:\n{dump_text}"
    );
    let required_features: &[&str] = match revision {
        Revision::Ext2 => &[
            "ext_attr",
            "dir_index",
            "filetype",
            "sparse_super",
            "large_file",
        ],
        Revision::Ext3 => &[
            "has_journal",
            "ext_attr",
            "dir_index",
            "filetype",
            "sparse_super",
            "large_file",
        ],
        Revision::Ext4 => &[
            "has_journal",
            "ext_attr",
            "dir_index",
            "filetype",
            "extent",
            "64bit",
            "flex_bg",
            "sparse_super",
            "large_file",
            "huge_file",
            "dir_nlink",
            "extra_isize",
            "metadata_csum",
        ],
    };
    for feature in required_features {
        assert!(
            dump_text.split_whitespace().any(|word| word == *feature),
            "missing {feature} feature:\n{dump_text}"
        );
    }

    let ls = e2fs(root, &["debugfs", "-R", "ls -l /", rel]);
    assert!(
        ls.status.success(),
        "debugfs failed: {}",
        String::from_utf8_lossy(&ls.stderr)
    );
    let listing = String::from_utf8_lossy(&ls.stdout);
    for entry in [".", "..", "lost+found"] {
        assert!(
            listing.contains(entry),
            "directory listing misses {entry}:\n{listing}"
        );
    }
}

#[test]
fn mkfs_oracle_small_revisions() {
    let root = workspace();
    if !oracle_available("mkfs oracle") {
        return;
    }
    for (name, revision) in [
        ("mkfs-ext2-4m", Revision::Ext2),
        ("mkfs-ext3-4m", Revision::Ext3),
        ("mkfs-ext4-4m", Revision::Ext4),
    ] {
        format_image(&root, name, 4 * 1024 * 1024, revision, 1024, 256);
        assert_usable(&root, &format!("target/img/{name}.img"), revision, 1024);
    }
}

#[test]
#[ignore = "oracle matrix creates large sparse images and requires Docker"]
fn mkfs_oracle_sparse_matrix_and_allocator() {
    let root = workspace();
    if !oracle_available("mkfs oracle") {
        return;
    }
    let cases = [
        (
            "mkfs-ext2-64m-2k",
            64 * 1024 * 1024,
            Revision::Ext2,
            2048,
            128,
        ),
        (
            "mkfs-ext3-64m-4k",
            64 * 1024 * 1024,
            Revision::Ext3,
            4096,
            256,
        ),
        (
            "mkfs-ext4-600m-1k",
            600 * 1024 * 1024,
            Revision::Ext4,
            1024,
            256,
        ),
        (
            "mkfs-ext4-5g-4k",
            5 * 1024 * 1024 * 1024,
            Revision::Ext4,
            4096,
            256,
        ),
        (
            "mkfs-ext4-40g-64k",
            40 * 1024 * 1024 * 1024,
            Revision::Ext4,
            65536,
            256,
        ),
    ];
    let payload = root.join("target/img/mkfs-oracle-payload");
    fs::write(&payload, b"ext allocation oracle\n").expect("write payload");
    for (name, bytes, revision, block_size, inode_size) in cases {
        format_image(&root, name, bytes, revision, block_size, inode_size);
        let image = format!("target/img/{name}.img");
        assert_usable(&root, &image, revision, block_size);
        let write = e2fs(
            &root,
            &[
                "debugfs",
                "-w",
                "-R",
                "write target/img/mkfs-oracle-payload name",
                &image,
            ],
        );
        assert!(
            write.status.success(),
            "debugfs write failed: {}",
            String::from_utf8_lossy(&write.stderr)
        );
        let fsck = e2fs(&root, &["e2fsck", "-fn", &image]);
        assert!(
            fsck.status.success(),
            "e2fsck after allocation failed:\n{}\n{}",
            String::from_utf8_lossy(&fsck.stdout),
            String::from_utf8_lossy(&fsck.stderr)
        );
    }
}
