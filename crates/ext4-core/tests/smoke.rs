//! End-to-end: Linux-made image, read with the engine, modify, verify with e2fsck.

mod support;
use support::*;

#[test]
fn read_then_write_round_trip() {
    if !oracle_or_skip("read_then_write_round_trip") {
        return;
    }
    let (abs, rel) = mke2fs("smoke", "64M", &["-t", "ext4", "-b", "4096", "-L", "smoke"]);
    let root = workspace();
    let small = pattern(5000, 1);
    let big = pattern(3 << 20, 2);
    std::fs::write(root.join("target/img/small.bin"), &small).unwrap();
    std::fs::write(root.join("target/img/big.bin"), &big).unwrap();
    let o = debugfs_w(
        &rel,
        "mkdir dir\nwrite target/img/small.bin dir/small\nwrite target/img/big.bin big\nsymlink link dir/small\n",
    );
    assert!(o.status.success(), "{}", text(&o));

    {
        let fs = mount(&abs, true);
        let names: Vec<String> = fs
            .list_dir(2)
            .unwrap()
            .into_iter()
            .map(|e| String::from_utf8_lossy(&e.name).into_owned())
            .collect();
        for n in [".", "..", "lost+found", "dir", "big", "link"] {
            assert!(names.contains(&n.to_string()), "missing {n} in {names:?}");
        }
        assert_eq!(read_all(&fs, resolve(&fs, "dir/small")), small);
        assert_eq!(read_all(&fs, resolve(&fs, "big")), big);
        assert_eq!(fs.readlink(resolve(&fs, "link")).unwrap(), b"dir/small");
    }

    {
        let fs = mount(&abs, false);
        assert!(!fs.is_read_only(), "{:?}", fs.read_only_reason());
        let d = fs.mkdir(2, b"newdir", 0o755, 501, 20).unwrap();
        let f = fs
            .create(d.ino, b"hello.txt", 0o100644, 501, 20, (0, 0))
            .unwrap();
        fs.write(f.ino, 0, b"hello from mac\n").unwrap();
        let data = pattern(1 << 20, 3);
        let g = fs.create(2, b"one-mib", 0o100644, 0, 0, (0, 0)).unwrap();
        fs.write(g.ino, 0, &data).unwrap();
        fs.symlink(2, b"long-link", &[b'x'; 100], 0, 0).unwrap();
        fs.rename(2, b"big", d.ino, b"big-moved", false, false)
            .unwrap();
        let removed = fs.remove(d.ino, b"big-moved").unwrap();
        fs.reclaim(removed).unwrap();
        fs.set_xattr(g.ino, b"com.apple.test", b"value", false, false)
            .unwrap();
        fs.unmount().unwrap();
    }
    assert_fsck_clean(&rel);
    let listing = debugfs_r(&rel, "cat newdir/hello.txt");
    assert!(listing.contains("hello from mac"), "{listing}");
    let fs = mount(&abs, true);
    assert_eq!(read_all(&fs, resolve(&fs, "one-mib")), pattern(1 << 20, 3));
    assert_eq!(
        fs.get_xattr(resolve(&fs, "one-mib"), b"com.apple.test")
            .unwrap(),
        b"value"
    );
}
