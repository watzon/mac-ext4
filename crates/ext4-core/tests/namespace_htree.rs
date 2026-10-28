//! Indexed directories: promotion of a full linear directory to an htree,
//! and the "." and ".." records in an htree root.

mod support;

use std::collections::HashMap;

use ext4_core::check::{check, CheckOptions};
use ext4_core::inode::FL_INDEX;
use ext4_core::mkfs::{format, MkfsOptions};
use ext4_core::{Error, FileDevice, Filesystem};

use support::*;

/// `e2fsck -fn` exits 0 and prints nothing beyond its pass banners.
fn assert_fsck_no_problems(rel: &str) {
    let o = e2fs(&["e2fsck", "-fn", rel]);
    let out = text(&o);
    assert!(o.status.success(), "e2fsck -fn {rel}:\n{out}");
    for bad in ["Fix?", "HTREE", "WARNING", "Clear?", "Problem"] {
        assert!(
            !out.contains(bad),
            "e2fsck -fn {rel} reported {bad}:\n{out}"
        );
    }
}

fn assert_own_check_clean(abs: &std::path::Path) {
    let report = check(
        Box::new(FileDevice::open(abs, false).unwrap()),
        &CheckOptions::default(),
        &mut |_| {},
        &mut |_| {},
    )
    .unwrap();
    assert!(report.clean, "{:?}", report.problems);
}

fn engine_image(name: &str, size: u64, block_size: u32) -> (std::path::PathBuf, String) {
    let rel = format!("target/img/{name}.img");
    let abs = workspace().join(&rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&abs);
    std::fs::File::create(&abs).unwrap().set_len(size).unwrap();
    format(
        &FileDevice::open(&abs, true).unwrap(),
        &MkfsOptions {
            block_size: Some(block_size),
            ..Default::default()
        },
        &mut |_| {},
    )
    .unwrap();
    (abs, rel)
}

fn is_indexed(fs: &Filesystem, dir: u32) -> bool {
    fs.getattr(dir).unwrap().flags & FL_INDEX != 0
}

/// Every name in `expect` appears exactly once, plus "." and "..".
fn assert_listing(fs: &Filesystem, dir: u32, expect: &HashMap<String, u32>) {
    let mut seen: HashMap<Vec<u8>, u32> = HashMap::new();
    for e in fs.list_dir(dir).unwrap() {
        *seen.entry(e.name.clone()).or_default() += 1;
        if let Some(&ino) = expect.get(&*String::from_utf8_lossy(&e.name)) {
            assert_eq!(e.inode, ino, "{}", String::from_utf8_lossy(&e.name));
        }
    }
    assert_eq!(seen.remove(&b"."[..]), Some(1));
    assert_eq!(seen.remove(&b".."[..]), Some(1));
    assert_eq!(seen.len(), expect.len(), "listing size");
    for name in expect.keys() {
        assert_eq!(seen.get(name.as_bytes()), Some(&1), "{name} listed once");
    }
}

/// Create files until the directory turns into an htree, check it at that
/// point, then keep going and mix in unlinks and renames.
fn exercise_promotion(abs: &std::path::Path, rel: &str, total: u32, expect_index: bool) {
    let fs = mount(abs, false);
    let dir = fs.mkdir(2, b"d", 0o755, 0, 0).unwrap().ino;
    let other = fs.mkdir(2, b"other", 0o755, 0, 0).unwrap().ino;
    let mut names: HashMap<String, u32> = HashMap::new();
    let mut promoted_at = None;
    for n in 0..total {
        let name = format!("file-{n:06}");
        let ino = fs
            .create(dir, name.as_bytes(), 0o100644, 0, 0, (0, 0))
            .unwrap()
            .ino;
        names.insert(name, ino);
        if promoted_at.is_none() && is_indexed(&fs, dir) {
            promoted_at = Some(n);
            let attr = fs.getattr(dir).unwrap();
            let bs = fs.statfs().block_size as u64;
            // Root plus the two leaves of the first split.
            assert_eq!(attr.size, 3 * bs);
            assert_listing(&fs, dir, &names);
            for (name, &ino) in &names {
                assert_eq!(fs.lookup(dir, name.as_bytes()).unwrap().ino, ino);
            }
            assert_eq!(fs.lookup(dir, b".").unwrap().ino, dir);
            assert_eq!(fs.lookup(dir, b"..").unwrap().ino, 2);
            assert_eq!(fs.getattr(dir).unwrap().parent, 2);
            fs.sync().unwrap();
        }
    }
    assert_eq!(promoted_at.is_some(), expect_index, "promotion");
    assert_eq!(is_indexed(&fs, dir), expect_index);
    assert_listing(&fs, dir, &names);

    // Unlink every third name.
    for n in (0..total).step_by(3) {
        let name = format!("file-{n:06}");
        let ino = fs.remove(dir, name.as_bytes()).unwrap();
        fs.reclaim(ino).unwrap();
        names.remove(&name);
    }
    // Rename inside the directory and out of it.
    for n in (1..total).step_by(7) {
        let name = format!("file-{n:06}");
        let Some(ino) = names.remove(&name) else {
            continue;
        };
        if n % 2 == 0 {
            let new = format!("renamed-{n:06}");
            fs.rename(dir, name.as_bytes(), dir, new.as_bytes(), false, false)
                .unwrap();
            names.insert(new, ino);
        } else {
            fs.rename(dir, name.as_bytes(), other, name.as_bytes(), false, false)
                .unwrap();
            assert_eq!(fs.lookup(other, name.as_bytes()).unwrap().ino, ino);
        }
    }
    for (name, &ino) in &names {
        assert_eq!(fs.lookup(dir, name.as_bytes()).unwrap().ino, ino);
    }
    assert!(matches!(
        fs.lookup(dir, b"file-000000"),
        Err(Error::NotFound)
    ));
    assert_listing(&fs, dir, &names);
    fs.unmount().unwrap();

    assert_fsck_no_problems(rel);
    assert_own_check_clean(abs);
    let htree = debugfs_r(rel, "htree d");
    assert_eq!(htree.contains("Hash Version"), expect_index, "{htree}");

    // The result still mounts and reads back after a remount.
    let fs = mount(abs, true);
    let dir = fs.lookup(2, b"d").unwrap().ino;
    assert_listing(&fs, dir, &names);
    for (name, &ino) in &names {
        assert_eq!(fs.lookup(dir, name.as_bytes()).unwrap().ino, ino);
    }
}

#[test]
fn namespace_htree_promotes_engine_formatted_directory() {
    if !oracle_or_skip("namespace_htree_promotes_engine_formatted_directory") {
        return;
    }
    let (abs, rel) = engine_image("namespace-htree-engine", 64 << 20, 4096);
    exercise_promotion(&abs, &rel, 3000, true);
}

#[test]
fn namespace_htree_promotes_small_blocks_without_checksums() {
    if !oracle_or_skip("namespace_htree_promotes_small_blocks_without_checksums") {
        return;
    }
    // 1 KiB blocks fill the root's 124 index slots, so this also grows the
    // tree to a second index level.
    let (abs, rel) = mke2fs(
        "namespace-htree-1k",
        "64M",
        &["-t", "ext4", "-b", "1024", "-O", "^metadata_csum"],
    );
    exercise_promotion(&abs, &rel, 6000, true);
    assert!(debugfs_r(&rel, "htree d").contains("Indirect levels: 1"));
}

#[test]
fn namespace_htree_promotes_after_inline_directory_grows() {
    if !oracle_or_skip("namespace_htree_promotes_after_inline_directory_grows") {
        return;
    }
    let (abs, rel) = mke2fs(
        "namespace-htree-inline",
        "64M",
        &["-t", "ext4", "-b", "4096", "-O", "inline_data"],
    );
    exercise_promotion(&abs, &rel, 1500, true);
}

#[test]
fn namespace_htree_stays_linear_without_dir_index() {
    if !oracle_or_skip("namespace_htree_stays_linear_without_dir_index") {
        return;
    }
    let (abs, rel) = mke2fs(
        "namespace-htree-nodx",
        "64M",
        &["-t", "ext4", "-b", "4096", "-O", "^dir_index"],
    );
    exercise_promotion(&abs, &rel, 600, false);
}

/// Linux-indexed directories: "." and ".." resolve, getattr reports the
/// parent, and an indexed directory moves between indexed parents.
#[test]
fn namespace_htree_dot_entries_and_moves_in_linux_indexed_dirs() {
    if !oracle_or_skip("namespace_htree_dot_entries_and_moves_in_linux_indexed_dirs") {
        return;
    }
    let (abs, rel) = mke2fs("namespace-htree-dots", "64M", &["-t", "ext4", "-b", "4096"]);
    let mut script = String::from("mkdir a\nmkdir b\nmkdir a/big\n");
    for dir in ["/a", "/b", "/a/big"] {
        script.push_str(&format!("cd {dir}\n"));
        for n in 0..400 {
            script.push_str(&format!("mknod seed-{n:04} p\n"));
        }
    }
    let out = debugfs_w(&rel, &script);
    assert!(out.status.success(), "{}", text(&out));
    let indexed = e2fs(&["e2fsck", "-fyD", &rel]);
    assert!(
        indexed.status.code().is_some_and(|code| code <= 1),
        "{}",
        text(&indexed)
    );
    for dir in ["a", "b", "a/big"] {
        assert!(debugfs_r(&rel, &format!("htree {dir}")).contains("Hash Version"));
    }

    let fs = mount(&abs, false);
    let a = resolve(&fs, "a");
    let b = resolve(&fs, "b");
    let big = resolve(&fs, "a/big");
    assert!(is_indexed(&fs, a) && is_indexed(&fs, b) && is_indexed(&fs, big));
    assert_eq!(fs.lookup(a, b".").unwrap().ino, a);
    assert_eq!(fs.lookup(a, b"..").unwrap().ino, 2);
    assert_eq!(fs.lookup(big, b"..").unwrap().ino, a);
    assert_eq!(fs.getattr(a).unwrap().parent, 2);
    assert_eq!(fs.getattr(big).unwrap().parent, a);
    let a_links = fs.getattr(a).unwrap().nlink;
    let b_links = fs.getattr(b).unwrap().nlink;

    // Moving "big" rewrites ".." inside its own htree root.
    fs.rename(a, b"big", b, b"big", false, false).unwrap();
    assert_eq!(fs.lookup(b, b"big").unwrap().ino, big);
    assert!(matches!(fs.lookup(a, b"big"), Err(Error::NotFound)));
    assert_eq!(fs.lookup(big, b"..").unwrap().ino, b);
    assert_eq!(fs.getattr(big).unwrap().parent, b);
    assert_eq!(fs.getattr(a).unwrap().nlink, a_links - 1);
    assert_eq!(fs.getattr(b).unwrap().nlink, b_links + 1);
    assert_eq!(fs.lookup(big, b"seed-0123").unwrap().ino, {
        let e = fs
            .list_dir(big)
            .unwrap()
            .into_iter()
            .find(|e| e.name == b"seed-0123")
            .unwrap();
        e.inode
    });
    fs.unmount().unwrap();

    assert_fsck_no_problems(&rel);
    assert_own_check_clean(&abs);
    // The rewritten root must still verify on a checksum-enforcing mount.
    let fs = mount(&abs, true);
    let big = resolve(&fs, "b/big");
    assert_eq!(fs.lookup(big, b"..").unwrap().ino, resolve(&fs, "b"));
}
