//! Indexed-directory interoperability with e2fsprogs-built htrees.

mod support;

use support::*;

#[test]
fn reads_e2fsck_built_htree_directories() {
    if !oracle_or_skip("reads_e2fsck_built_htree_directories") {
        return;
    }
    let (abs, rel) = mke2fs("interop-htree", "128M", &["-t", "ext4", "-b", "4096"]);
    let root = workspace();
    std::fs::write(root.join("target/img/interop-htree-byte"), [0x5A]).unwrap();
    let mut script = String::from("mkdir entries\n");
    for index in 0..2_000 {
        script.push_str(&format!(
            "write target/img/interop-htree-byte entries/item-{index:05}\n"
        ));
    }
    let output = debugfs_w(&rel, &script);
    assert!(output.status.success(), "{}", text(&output));
    let optimize = e2fs(&["e2fsck", "-fyD", &rel]);
    assert!(
        optimize.status.code().is_some_and(|code| code <= 1),
        "{}",
        text(&optimize)
    );
    let htree = debugfs_r(&rel, "htree entries");
    assert!(htree.contains("Hash Version"), "{htree}");

    let fs = mount(&abs, true);
    let dir = fs
        .list_dir(2)
        .unwrap()
        .into_iter()
        .find(|entry| entry.name == b"entries")
        .unwrap()
        .inode;
    assert_eq!(fs.lookup(2, b"entries").unwrap().ino, dir);

    assert_eq!(fs.list_dir(dir).unwrap().len(), 2_002);
    for index in [0, 1, 17, 999, 1_337, 1_999] {
        let name = format!("item-{index:05}");
        assert_eq!(
            read_all(&fs, fs.lookup(dir, name.as_bytes()).unwrap().ino),
            [0x5A]
        );
    }
}
