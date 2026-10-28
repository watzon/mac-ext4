//! Namespace operation regressions: link of a reclaimed inode, rename link
//! counts, exchange permissions, and the setattr flag mask.

mod support;

use ext4_core::check::{check, CheckOptions};
use ext4_core::inode::{FL_APPEND, FL_EXTENTS, FL_IMMUTABLE, FL_NOATIME, FL_NODUMP};
use ext4_core::ops::SetAttr;
use ext4_core::{Error, FileDevice, Filesystem};

use support::*;

/// `e2fsck -fn` exits 0 and prints nothing beyond its pass banners.
fn assert_fsck_no_problems(rel: &str) {
    let o = e2fs(&["e2fsck", "-fn", rel]);
    let out = text(&o);
    assert!(o.status.success(), "e2fsck -fn {rel}:\n{out}");
    for bad in ["Fix?", "WARNING", "Clear?", "count is", "should be"] {
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

fn nlink(fs: &Filesystem, ino: u32) -> u32 {
    fs.getattr(ino).unwrap().nlink
}

#[test]
fn namespace_link_rejects_reclaimed_inode() {
    if !oracle_or_skip("namespace_link_rejects_reclaimed_inode") {
        return;
    }
    let (abs, rel) = mke2fs("namespace-link-dead", "32M", &["-t", "ext4", "-b", "4096"]);
    let fs = mount(&abs, false);

    // A removed but still referenced inode is a retained orphan and may be
    // linked back.
    let open = fs.create(2, b"open", 0o100644, 0, 0, (0, 0)).unwrap().ino;
    assert_eq!(fs.remove(2, b"open").unwrap(), open);
    assert_eq!(fs.link(open, 2, b"open-again").unwrap().nlink, 1);

    // Once reclaimed, the inode is free and must not come back.
    let dead = fs.create(2, b"dead", 0o100644, 0, 0, (0, 0)).unwrap().ino;
    fs.remove(2, b"dead").unwrap();
    fs.reclaim(dead).unwrap();
    assert!(matches!(fs.link(dead, 2, b"zombie"), Err(Error::NotFound)));
    assert!(matches!(fs.lookup(2, b"zombie"), Err(Error::NotFound)));

    // The next allocation may reuse the number without aliasing a name.
    let fresh = fs.create(2, b"fresh", 0o100644, 0, 0, (0, 0)).unwrap();
    assert_eq!(fresh.nlink, 1);
    fs.unmount().unwrap();
    assert_fsck_no_problems(&rel);
    assert_own_check_clean(&abs);
}

#[test]
fn namespace_rename_link_count_matrix() {
    if !oracle_or_skip("namespace_rename_link_count_matrix") {
        return;
    }
    let (abs, rel) = mke2fs(
        "namespace-rename-links",
        "32M",
        &["-t", "ext4", "-b", "4096"],
    );
    let fs = mount(&abs, false);
    let mk = |parent: u32, name: &str| fs.mkdir(parent, name.as_bytes(), 0o755, 0, 0).unwrap().ino;
    let file = |parent: u32, name: &str| {
        fs.create(parent, name.as_bytes(), 0o100644, 0, 0, (0, 0))
            .unwrap()
            .ino
    };

    // Directory, same parent, no replacement: parent unchanged.
    let p = mk(2, "p1");
    let a = mk(p, "a");
    fs.rename(p, b"a", p, b"b", false, false).unwrap();
    assert_eq!(nlink(&fs, p), 3);
    assert_eq!(nlink(&fs, a), 2);
    assert_eq!(fs.lookup(a, b"..").unwrap().ino, p);

    // Directory over an empty directory, same parent: one subdir fewer.
    let p = mk(2, "p2");
    let a = mk(p, "a");
    let victim = mk(p, "b");
    assert_eq!(nlink(&fs, p), 4);
    fs.rename(p, b"a", p, b"b", false, false).unwrap();
    assert_eq!(nlink(&fs, p), 3);
    assert_eq!(fs.lookup(p, b"b").unwrap().ino, a);
    fs.reclaim(victim).unwrap();

    // Directory, cross parent, no replacement: source -1, destination +1.
    let s = mk(2, "s3");
    let d = mk(2, "d3");
    let a = mk(s, "a");
    fs.rename(s, b"a", d, b"a", false, false).unwrap();
    assert_eq!((nlink(&fs, s), nlink(&fs, d)), (2, 3));
    assert_eq!(fs.lookup(a, b"..").unwrap().ino, d);

    // Directory over an empty directory, cross parent: source -1,
    // destination unchanged (one subdir leaves, one arrives).
    let s = mk(2, "s4");
    let d = mk(2, "d4");
    let a = mk(s, "a");
    let victim = mk(d, "b");
    assert_eq!((nlink(&fs, s), nlink(&fs, d)), (3, 3));
    fs.rename(s, b"a", d, b"b", false, false).unwrap();
    assert_eq!((nlink(&fs, s), nlink(&fs, d)), (2, 3));
    assert_eq!(fs.lookup(a, b"..").unwrap().ino, d);
    fs.reclaim(victim).unwrap();

    // Files never change parent link counts.
    let p = mk(2, "p5");
    let f = file(p, "f");
    let g = file(p, "g");
    fs.rename(p, b"f", p, b"h", false, false).unwrap();
    fs.rename(p, b"h", p, b"g", false, false).unwrap();
    assert_eq!(nlink(&fs, p), 2);
    assert_eq!(nlink(&fs, f), 1);
    fs.reclaim(g).unwrap();
    let s = mk(2, "s6");
    let d = mk(2, "d6");
    let f = file(s, "f");
    let g = file(d, "g");
    fs.rename(s, b"f", d, b"f", false, false).unwrap();
    let f2 = file(s, "f2");
    fs.rename(s, b"f2", d, b"g", false, false).unwrap();
    assert_eq!((nlink(&fs, s), nlink(&fs, d)), (2, 2));
    assert_eq!((nlink(&fs, f), nlink(&fs, f2)), (1, 1));
    fs.reclaim(g).unwrap();

    fs.unmount().unwrap();
    assert_fsck_no_problems(&rel);
    assert_own_check_clean(&abs);
}

#[test]
fn namespace_rename_keeps_dir_nlink_sentinel() {
    if !oracle_or_skip("namespace_rename_keeps_dir_nlink_sentinel") {
        return;
    }
    let (abs, rel) = mke2fs(
        "namespace-rename-nlink1",
        "32M",
        &["-t", "ext4", "-b", "4096"],
    );
    let out = debugfs_w(
        &rel,
        "mkdir s\nmkdir d\nmkdir s/a\nmkdir s/x\nmkdir d/b\nsif s links_count 1\nsif d links_count 1",
    );
    assert!(out.status.success(), "{}", text(&out));
    let fs = mount(&abs, false);
    let s = resolve(&fs, "s");
    let d = resolve(&fs, "d");
    // A count of 1 means "too many to count" and must stay 1 both ways.
    fs.rename(s, b"a", d, b"a", false, false).unwrap();
    assert_eq!((nlink(&fs, s), nlink(&fs, d)), (1, 1));
    fs.rename(s, b"x", d, b"b", false, false).unwrap();
    assert_eq!((nlink(&fs, s), nlink(&fs, d)), (1, 1));
    fs.rename(d, b"a", d, b"c", false, false).unwrap();
    assert_eq!(nlink(&fs, d), 1);
}

#[test]
fn namespace_exchange_checks_both_inodes() {
    if !oracle_or_skip("namespace_exchange_checks_both_inodes") {
        return;
    }
    let (abs, rel) = mke2fs(
        "namespace-exchange-perm",
        "32M",
        &["-t", "ext4", "-b", "4096"],
    );
    let fs = mount(&abs, false);
    for flag in [FL_IMMUTABLE, FL_APPEND] {
        for protect_source in [false, true] {
            let x = fs.create(2, b"x", 0o100644, 0, 0, (0, 0)).unwrap().ino;
            let y = fs.create(2, b"y", 0o100644, 0, 0, (0, 0)).unwrap().ino;
            let protected = if protect_source { x } else { y };
            fs.setattr(
                protected,
                &SetAttr {
                    flags: Some(flag),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(
                matches!(
                    fs.rename(2, b"x", 2, b"y", false, true),
                    Err(Error::PermissionDenied)
                ),
                "flag {flag:#x}, source {protect_source}"
            );
            assert_eq!(fs.lookup(2, b"x").unwrap().ino, x);
            assert_eq!(fs.lookup(2, b"y").unwrap().ino, y);
            fs.setattr(
                protected,
                &SetAttr {
                    flags: Some(0),
                    ..Default::default()
                },
            )
            .unwrap();
            fs.rename(2, b"x", 2, b"y", false, true).unwrap();
            assert_eq!(fs.lookup(2, b"x").unwrap().ino, y);
            assert_eq!(fs.lookup(2, b"y").unwrap().ino, x);
            for name in [&b"x"[..], b"y"] {
                let ino = fs.remove(2, name).unwrap();
                fs.reclaim(ino).unwrap();
            }
        }
    }
    fs.unmount().unwrap();
    assert_fsck_no_problems(&rel);
}

#[test]
fn namespace_setattr_flags_keep_linux_only_flags() {
    if !oracle_or_skip("namespace_setattr_flags_keep_linux_only_flags") {
        return;
    }
    let (abs, rel) = mke2fs(
        "namespace-setattr-flags",
        "32M",
        &["-t", "ext4", "-b", "4096"],
    );
    let out = debugfs_w(
        &rel,
        &format!(
            "write /dev/null f\nsif f flags {:#x}",
            FL_EXTENTS | FL_NOATIME
        ),
    );
    assert!(out.status.success(), "{}", text(&out));
    let fs = mount(&abs, false);
    let f = resolve(&fs, "f");
    let attr = fs
        .setattr(
            f,
            &SetAttr {
                flags: Some(FL_NODUMP),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(attr.flags, FL_EXTENTS | FL_NOATIME | FL_NODUMP);
    // The host cannot set noatime either.
    let attr = fs
        .setattr(
            f,
            &SetAttr {
                flags: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(attr.flags, FL_EXTENTS | FL_NOATIME);
    fs.unmount().unwrap();
    assert_fsck_no_problems(&rel);
}
