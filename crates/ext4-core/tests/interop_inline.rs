//! Writes to Linux-made inline_data files and directories must convert them
//! to block-mapped form in a way e2fsck accepts.

mod support;
use support::*;

#[test]
fn writes_convert_inline_files_and_directories() {
    if !oracle_or_skip("writes_convert_inline_files_and_directories") {
        return;
    }
    let (abs, rel) = mke2fs(
        "interop-inline-write",
        "32M",
        &["-t", "ext4", "-b", "4096", "-O", "inline_data"],
    );
    let root = workspace();
    let small = pattern(40, 7);
    std::fs::write(root.join("target/img/inline-small.bin"), &small).unwrap();
    let o = debugfs_w(
        &rel,
        "mkdir d\nwrite target/img/inline-small.bin d/f\nwrite target/img/inline-small.bin g\n",
    );
    assert!(o.status.success(), "{}", text(&o));
    let stat = debugfs_r(&rel, "stat d/f");
    assert!(
        stat.contains("Inline data") || stat.contains("Size of inline data"),
        "file is not inline: {stat}"
    );
    let dstat = debugfs_r(&rel, "stat d");
    assert!(
        dstat.contains("Inline data") || dstat.contains("Size of inline data"),
        "dir is not inline: {dstat}"
    );

    {
        let fs = mount(&abs, false);
        assert!(!fs.is_read_only(), "{:?}", fs.read_only_reason());
        let d = resolve(&fs, "d");
        let f = resolve(&fs, "d/f");
        // Append past the inline capacity: the file moves to blocks.
        let tail = pattern(10_000, 8);
        fs.write(f, small.len() as u64, &tail).unwrap();
        // Add enough entries to overflow the inline directory.
        for i in 0..40 {
            fs.create(
                d,
                format!("entry-{i:03}").as_bytes(),
                0o100644,
                0,
                0,
                (0, 0),
            )
            .unwrap();
        }
        // Small in-place overwrite of the other inline file.
        let g = resolve(&fs, "g");
        fs.write(g, 0, b"XY").unwrap();
        fs.unmount().unwrap();
    }
    assert_fsck_clean(&rel);

    let fs = mount(&abs, true);
    let mut expect = small.clone();
    expect.extend(pattern(10_000, 8));
    assert_eq!(read_all(&fs, resolve(&fs, "d/f")), expect);
    let mut g = small.clone();
    g[..2].copy_from_slice(b"XY");
    assert_eq!(read_all(&fs, resolve(&fs, "g")), g);
    let names: Vec<Vec<u8>> = fs
        .list_dir(resolve(&fs, "d"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names.len(), 2 + 1 + 40);
    let listing = debugfs_r(&rel, "ls d");
    assert!(listing.contains("entry-039"), "{listing}");
}
