//! Read interoperability across ext-family layouts made by e2fsprogs.

mod support;

use ext4_core::inode::FileType;
use support::*;

fn install_read_fixture(rel: &str, small: &[u8], large: &[u8]) {
    let root = workspace();
    std::fs::write(root.join("target/img/interop-read-small.bin"), small).unwrap();
    std::fs::write(root.join("target/img/interop-read-large.bin"), large).unwrap();
    let script = concat!(
        "mkdir data\n",
        "write target/img/interop-read-small.bin data/small\n",
        "write target/img/interop-read-large.bin data/large\n",
        "symlink fast target\n",
        "symlink slow ",
    );
    let mut commands = String::from(script);
    commands.push_str(&"x".repeat(180));
    commands.push_str(
        "\nwrite target/img/interop-read-small.bin data/sparse\nea_set data/small user.note read-value\nfallocate data/sparse 100 100\nmknod null c 1 3\n",
    );
    let output = debugfs_w(rel, &commands);
    assert!(output.status.success(), "{}", text(&output));
}

#[test]
fn reads_linux_made_ext2_ext3_and_ext4_feature_variants() {
    if !oracle_or_skip("reads_linux_made_ext2_ext3_and_ext4_feature_variants") {
        return;
    }
    let small = pattern(1537, 11);
    let large = pattern((1 << 20) + 777, 12);
    let variants: [(&str, &str, &str, &[&str]); 8] = [
        ("interop-read-ext2", "32M", "ext2", &["-b", "1024"]),
        ("interop-read-ext3", "32M", "ext3", &["-b", "1024"]),
        ("interop-read-ext4-1k", "32M", "ext4", &["-b", "1024"]),
        ("interop-read-ext4-2k", "32M", "ext4", &["-b", "2048"]),
        ("interop-read-ext4-4k", "64M", "ext4", &["-b", "4096"]),
        ("interop-read-ext4-64k", "256M", "ext4", &["-b", "65536"]),
        (
            "interop-read-no-csum",
            "64M",
            "ext4",
            &["-b", "4096", "-O", "^metadata_csum"],
        ),
        (
            "interop-read-no-64",
            "64M",
            "ext4",
            &["-b", "4096", "-O", "^64bit"],
        ),
    ];
    for (name, size, fs_type, args) in variants {
        let mut mkfs = vec!["-t", fs_type];
        mkfs.extend_from_slice(args);
        let (abs, rel) = mke2fs(name, size, &mkfs);
        install_read_fixture(&rel, &small, &large);
        let fs = mount(&abs, true);
        assert_eq!(read_all(&fs, resolve(&fs, "data/small")), small, "{name}");
        assert_eq!(read_all(&fs, resolve(&fs, "data/large")), large, "{name}");
        assert_eq!(
            fs.readlink(resolve(&fs, "fast")).unwrap(),
            b"target",
            "{name}"
        );
        assert_eq!(
            fs.readlink(resolve(&fs, "slow")).unwrap(),
            vec![b'x'; 180],
            "{name}"
        );
        assert_eq!(
            fs.get_xattr(resolve(&fs, "data/small"), b"note").unwrap(),
            b"read-value",
            "{name}"
        );
        assert_eq!(
            fs.getattr(resolve(&fs, "null")).unwrap().file_type,
            FileType::CharDevice,
            "{name}"
        );
        assert!(
            debugfs_r(&rel, "stat data/large").contains("Inode:"),
            "{name}"
        );
    }
}

#[test]
fn reads_inline_data_files_created_by_debugfs() {
    if !oracle_or_skip("reads_inline_data_files_created_by_debugfs") {
        return;
    }
    let data = pattern(40, 21);
    let root = workspace();
    std::fs::write(root.join("target/img/interop-inline.bin"), &data).unwrap();
    let (abs, rel) = mke2fs(
        "interop-read-inline",
        "32M",
        &["-t", "ext4", "-O", "inline_data"],
    );
    let output = debugfs_w(&rel, "write target/img/interop-inline.bin inline\n");
    assert!(output.status.success(), "{}", text(&output));
    let fs = mount(&abs, true);
    assert_eq!(read_all(&fs, resolve(&fs, "inline")), data);
}
