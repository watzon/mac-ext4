//! Stress indexed-directory insertion through the engine.

mod support;

use support::*;

#[test]
fn engine_splits_linux_htree_leaves_under_many_long_names() {
    if !oracle_or_skip("engine_splits_linux_htree_leaves_under_many_long_names") {
        return;
    }
    let (abs, rel) = mke2fs("interop-htree-write", "256M", &["-t", "ext4", "-b", "4096"]);
    let root = workspace();
    std::fs::write(root.join("target/img/interop-htree-write-byte"), [0x4d]).unwrap();
    let mut script = String::from("mkdir dir\n");
    for n in 0..256 {
        script.push_str(&format!(
            "write target/img/interop-htree-write-byte dir/seed-{n:04}\n"
        ));
    }
    let output = debugfs_w(&rel, &script);
    assert!(output.status.success(), "{}", text(&output));
    let indexed = e2fs(&["e2fsck", "-fyD", &rel]);
    assert!(
        indexed.status.code().is_some_and(|code| code <= 1),
        "{}",
        text(&indexed)
    );
    assert!(debugfs_r(&rel, "htree dir").contains("Hash Version"));

    let fs = mount(&abs, false);
    let dir = fs.lookup(2, b"dir").unwrap().ino;
    let suffix = "x".repeat(210);
    for n in 0..20_000u32 {
        let name = format!("entry-{n:05}-{suffix}");
        fs.create(dir, name.as_bytes(), 0o100644, 0, 0, (0, 0))
            .unwrap();
        if n % 2_000 == 1_999 {
            fs.sync().unwrap();
            for probe in [0, n / 2, n] {
                let name = format!("entry-{probe:05}-{suffix}");
                fs.lookup(dir, name.as_bytes()).unwrap();
            }
        }
    }
    fs.sync().unwrap();
    for n in (0..20_000u32).step_by(97) {
        let name = format!("entry-{n:05}-{suffix}");
        fs.lookup(dir, name.as_bytes()).unwrap();
    }
    for n in (0..20_000u32).step_by(2) {
        let name = format!("entry-{n:05}-{suffix}");
        let removed = fs.remove(dir, name.as_bytes()).unwrap();
        fs.reclaim(removed).unwrap();
    }
    fs.sync().unwrap();
    for n in (1..20_000u32).step_by(194) {
        let name = format!("entry-{n:05}-{suffix}");
        fs.lookup(dir, name.as_bytes()).unwrap();
    }
    fs.unmount().unwrap();
    assert_fsck_clean(&rel);
    assert!(debugfs_r(&rel, "htree dir").contains("Hash Version"));
}
