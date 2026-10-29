//! Kernel-offloaded I/O mappings: the engine maps file ranges, "the kernel"
//! (this test) moves the data straight to the device, then reports the
//! write complete. Linux e2fsck must accept every state in between.

mod support;

use std::fs::{self, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

use ext4_core::koio::{IoKind, IoRun};
use ext4_core::ops::SetAttr;
use ext4_core::{Error, Filesystem};
use support::*;

/// Assert `e2fsck -fn` ran every pass and printed nothing else.
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

/// Map `data` at `offset` for writing and put it on the device the way the
/// kernel does: new (unwritten) blocks are written whole, zero-filled past
/// the data. Does not complete the write. Returns the bytes covered.
fn kernel_write_data(fs: &Filesystem, image: &Path, ino: u32, offset: u64, data: &[u8]) -> u64 {
    let bs = fs.statfs().block_size as u64;
    let dev = OpenOptions::new().write(true).open(image).unwrap();
    let runs = fs
        .map_for_write(ino, offset, data.len() as u64, 256)
        .unwrap();
    assert!(!runs.is_empty());
    assert_eq!(runs[0].logical, offset, "first run starts at the request");
    let mut covered = 0;
    for r in &runs {
        assert_ne!(r.kind, IoKind::Hole, "write mappings never contain holes");
        assert_eq!(r.physical % bs, r.logical % bs, "block-aligned mapping");
        let from = r.logical.max(offset);
        let to = (r.logical + r.len).min(offset + data.len() as u64);
        if from >= to {
            continue;
        }
        let mut chunk = data[(from - offset) as usize..(to - offset) as usize].to_vec();
        if r.kind == IoKind::Unwritten {
            // The kernel writes whole pages; a new block's tail is zeros.
            let fill_to = (to.div_ceil(bs) * bs).min(r.logical + r.len);
            chunk.resize((fill_to - from) as usize, 0);
        }
        dev.write_all_at(&chunk, r.physical + (from - r.logical))
            .unwrap();
        covered += to - from;
    }
    dev.sync_all().unwrap();
    covered
}

fn kernel_write(fs: &Filesystem, image: &Path, ino: u32, offset: u64, data: &[u8]) {
    let n = kernel_write_data(fs, image, ino, offset, data);
    assert_eq!(n, data.len() as u64, "the whole write was mapped");
    fs.complete_kernel_write(ino, offset, n).unwrap();
}

fn create(fs: &Filesystem, name: &str) -> u32 {
    fs.create(2, name.as_bytes(), 0o100644, 0, 0, (0, 0))
        .unwrap()
        .ino
}

fn kinds(runs: &[IoRun]) -> Vec<IoKind> {
    runs.iter().map(|r| r.kind).collect()
}

fn extent_lines(rel: &str, path: &str) -> usize {
    debugfs_r(rel, &format!("ex {path}"))
        .lines()
        .filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        .count()
}

#[test]
fn koio_sequential_writes_make_contiguous_extents() {
    if !oracle_or_skip("koio sequential writes") {
        return;
    }
    let (abs, rel) = mke2fs("koio-seq", "128M", &["-t", "ext4", "-b", "4096"]);
    let data = pattern(24 << 20, 1);
    {
        let fs = mount(&abs, false);
        let ino = create(&fs, "f");
        // 256 KiB I/O clusters, as a sequential copy produces.
        for (i, chunk) in data.chunks(256 << 10).enumerate() {
            kernel_write(&fs, &abs, ino, (i * (256 << 10)) as u64, chunk);
        }
        assert_eq!(fs.getattr(ino).unwrap().size, data.len() as u64);
        assert_eq!(read_all(&fs, ino), data);
        // Reads map to initialized data only.
        let runs = fs.map_for_read(ino, 0, data.len() as u64, 256).unwrap();
        assert!(runs.iter().all(|r| r.kind == IoKind::Data), "{runs:?}");
        fs.unmount().unwrap();
    }
    assert_fsck_silent(&rel);
    // Completed appends merge: 24 MiB fits in one initialized extent
    // (32768 blocks), so a well-placed file has very few.
    let n = extent_lines(&rel, "/f");
    assert!(n <= 2, "expected merged extents, debugfs shows {n} lines");
    let fs = mount(&abs, true);
    assert_eq!(read_all(&fs, resolve(&fs, "/f")), data);
}

#[test]
fn koio_crash_before_completion_exposes_nothing() {
    if !oracle_or_skip("koio crash before completion") {
        return;
    }
    let (abs, rel) = mke2fs("koio-crash", "64M", &["-t", "ext4", "-b", "4096"]);
    let first = pattern(1 << 20, 2);
    let second = pattern(1 << 20, 3);
    let crash_rel = "target/img/koio-crash-copy.img".to_string();
    let crash_abs = workspace().join(&crash_rel);
    {
        let fs = mount(&abs, false);
        let ino = create(&fs, "f");
        kernel_write(&fs, &abs, ino, 0, &first);
        // The second write reaches the disk, its blocks are allocated and
        // committed, but the completion never arrives.
        kernel_write_data(&fs, &abs, ino, first.len() as u64, &second);
        fs.sync().unwrap();
        fs::copy(&abs, &crash_abs).unwrap();
        fs.complete_kernel_write(ino, first.len() as u64, second.len() as u64)
            .unwrap();
        fs.unmount().unwrap();
    }
    // The copy is the disk as it was at the crash (still flagged as needing
    // journal recovery, which mounting does).
    {
        let fs = mount(&crash_abs, false);
        let ino = resolve(&fs, "/f");
        assert_eq!(fs.getattr(ino).unwrap().size, first.len() as u64);
        assert_eq!(read_all(&fs, ino), first);
        // The never-completed blocks stay unwritten past EOF.
        let tail = fs
            .map_for_read(ino, first.len() as u64, second.len() as u64, 16)
            .unwrap();
        assert_eq!(kinds(&tail), vec![IoKind::Unwritten]);
        fs.unmount().unwrap();
    }
    assert_fsck_silent(&crash_rel);
    // Without the crash, both writes are there.
    assert_fsck_silent(&rel);
    let fs = mount(&abs, true);
    let mut both = first.clone();
    both.extend_from_slice(&second);
    assert_eq!(read_all(&fs, resolve(&fs, "/f")), both);
    let _ = fs::remove_file(crash_abs);
}

#[test]
fn koio_fills_a_hole_inside_the_file() {
    if !oracle_or_skip("koio hole fill") {
        return;
    }
    let (abs, rel) = mke2fs("koio-hole", "64M", &["-t", "ext4", "-b", "4096"]);
    let size = 8u64 << 20;
    let at = 3u64 << 20;
    let data = pattern(256 << 10, 4);
    {
        let fs = mount(&abs, false);
        let ino = create(&fs, "sparse");
        fs.setattr(
            ino,
            &SetAttr {
                size: Some(size),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            kinds(&fs.map_for_read(ino, 0, size, 16).unwrap()),
            vec![IoKind::Hole]
        );

        kernel_write_data(&fs, &abs, ino, at, &data);
        // Allocated but not complete: still reads as zeros.
        let mid = fs.map_for_read(ino, at, data.len() as u64, 16).unwrap();
        assert_eq!(kinds(&mid), vec![IoKind::Unwritten]);
        assert!(read_all(&fs, ino).iter().all(|&b| b == 0));

        fs.complete_kernel_write(ino, at, data.len() as u64)
            .unwrap();
        let all = fs.map_for_read(ino, 0, size, 16).unwrap();
        assert_eq!(kinds(&all), vec![IoKind::Hole, IoKind::Data, IoKind::Hole]);
        assert_eq!(all[1].logical, at);
        assert_eq!(
            fs.getattr(ino).unwrap().size,
            size,
            "a fill inside EOF keeps the size"
        );
        let back = read_all(&fs, ino);
        assert_eq!(&back[at as usize..at as usize + data.len()], &data[..]);
        assert!(back[..at as usize].iter().all(|&b| b == 0));
        assert!(back[at as usize + data.len()..].iter().all(|&b| b == 0));
        fs.unmount().unwrap();
    }
    assert_fsck_silent(&rel);
}

#[test]
fn koio_partial_completion_grows_the_file_in_steps() {
    if !oracle_or_skip("koio partial completion") {
        return;
    }
    let (abs, rel) = mke2fs("koio-partial", "64M", &["-t", "ext4", "-b", "4096"]);
    let data = pattern(1 << 20, 5);
    let half = (data.len() / 2) as u64;
    {
        let fs = mount(&abs, false);
        let ino = create(&fs, "f");
        kernel_write_data(&fs, &abs, ino, 0, &data);
        fs.complete_kernel_write(ino, 0, half).unwrap();
        assert_eq!(fs.getattr(ino).unwrap().size, half);
        let rest = fs.map_for_read(ino, half, half, 16).unwrap();
        assert_eq!(kinds(&rest), vec![IoKind::Unwritten]);
        fs.unmount().unwrap();
    }
    assert_fsck_silent(&rel);
    {
        let fs = mount(&abs, false);
        let ino = resolve(&fs, "/f");
        // A completion that arrives through a cached mapping, without a
        // new blockmap (Apple's FAT module documents this case).
        fs.complete_kernel_write(ino, half, half).unwrap();
        assert_eq!(read_all(&fs, ino), data);
        fs.unmount().unwrap();
    }
    assert_fsck_silent(&rel);
}

#[test]
fn koio_stops_at_a_full_volume_and_keeps_what_fits() {
    if !oracle_or_skip("koio enospc") {
        return;
    }
    let (abs, rel) = mke2fs("koio-full", "16M", &["-t", "ext4", "-b", "4096"]);
    {
        let fs = mount(&abs, false);
        let ino = create(&fs, "big");
        let runs = fs.map_for_write(ino, 0, 64 << 20, 256).unwrap();
        let mapped: u64 = runs.iter().map(|r| r.len).sum();
        assert!(
            mapped > 0 && mapped < 64 << 20,
            "partial mapping, got {mapped}"
        );
        assert_eq!(runs.iter().map(|r| r.logical).min(), Some(0));
        let ino2 = create(&fs, "none");
        assert!(matches!(
            fs.map_for_write(ino2, 0, 1 << 20, 16),
            Err(Error::NoSpace)
        ));
        fs.unmount().unwrap();
    }
    // Everything mapped but never written is unwritten past EOF.
    assert_fsck_silent(&rel);
}

#[test]
fn koio_serves_block_mapped_files_with_zeroed_blocks() {
    if !oracle_or_skip("koio block-mapped files") {
        return;
    }
    let (abs, rel) = mke2fs("koio-ext3", "64M", &["-t", "ext3"]);
    let data = pattern(3 << 20, 6);
    {
        let fs = mount(&abs, false);
        let ino = create(&fs, "f");
        assert!(fs.getattr(ino).unwrap().kernel_io);
        for (i, chunk) in data.chunks(512 << 10).enumerate() {
            kernel_write(&fs, &abs, ino, (i * (512 << 10)) as u64, chunk);
        }
        assert_eq!(read_all(&fs, ino), data);
        // A mapped range that the kernel never writes reads as zeros: the
        // blocks were zeroed before they were mapped.
        let hole = create(&fs, "hole");
        fs.setattr(
            hole,
            &SetAttr {
                size: Some(1 << 20),
                ..Default::default()
            },
        )
        .unwrap();
        let mapped = fs.map_for_write(hole, 256 << 10, 64 << 10, 16).unwrap();
        assert!(mapped.iter().all(|r| r.kind == IoKind::Data));
        fs.sync().unwrap();
        assert!(read_all(&fs, hole).iter().all(|&b| b == 0));
        fs.unmount().unwrap();
    }
    assert_fsck_silent(&rel);
}

#[test]
fn koio_moves_inline_data_out_on_first_mapping() {
    if !oracle_or_skip("koio inline data") {
        return;
    }
    let (abs, rel) = mke2fs("koio-inline", "32M", &["-t", "ext4", "-O", "inline_data"]);
    let small = b"kept in the inode".to_vec();
    // debugfs stores small files inline (the engine never creates them).
    let src = "target/img/koio-inline-src.txt";
    fs::write(workspace().join(src), &small).unwrap();
    let o = debugfs_w(
        &rel,
        &format!("write {src} f\nwrite {src} w\nwrite {src} stays-inline\n"),
    );
    assert!(o.status.success(), "{}", text(&o));
    {
        let fs = mount(&abs, false);
        let ino = resolve(&fs, "/f");
        assert!(fs.getattr(ino).unwrap().flags & ext4_core::inode::FL_INLINE_DATA != 0);
        // Writable volume: inline files take the kernel path too.
        assert!(fs.getattr(ino).unwrap().kernel_io);
        let runs = fs.map_for_read(ino, 0, small.len() as u64, 4).unwrap();
        assert_eq!(kinds(&runs), vec![IoKind::Data]);
        assert_eq!(
            fs.getattr(ino).unwrap().flags & ext4_core::inode::FL_INLINE_DATA,
            0
        );
        assert_eq!(read_all(&fs, ino), small);
        let more = pattern(64 << 10, 7);
        kernel_write(&fs, &abs, ino, 4096, &more);
        let back = read_all(&fs, ino);
        assert_eq!(&back[..small.len()], &small[..]);
        assert_eq!(&back[4096..], &more[..]);
        // An inline file mapped for writing moves out the same way.
        let w = resolve(&fs, "/w");
        kernel_write(&fs, &abs, w, 0, &pattern(8192, 8));
        assert_eq!(read_all(&fs, w), pattern(8192, 8));
        fs.unmount().unwrap();
    }
    assert_fsck_silent(&rel);
    // Read-only: inline files stay out of the kernel path, others map.
    let fs = mount(&abs, true);
    let keep = resolve(&fs, "/stays-inline");
    assert!(!fs.getattr(keep).unwrap().kernel_io);
    assert!(fs.map_for_read(keep, 0, 16, 4).is_err());
    assert_eq!(read_all(&fs, keep), small);
    let f = resolve(&fs, "/f");
    assert!(fs.getattr(f).unwrap().kernel_io);
    assert!(fs.map_for_read(f, 0, 4096, 4).is_ok());
    assert!(matches!(
        fs.map_for_write(f, 0, 4096, 4),
        Err(Error::ReadOnly)
    ));
}

#[test]
fn koio_maps_only_regular_files() {
    if !oracle_or_skip("koio regular files") {
        return;
    }
    let (abs, _rel) = mke2fs("koio-dir", "32M", &["-t", "ext4"]);
    let fs = mount(&abs, false);
    let dir = fs.mkdir(2, b"d", 0o755, 0, 0).unwrap();
    assert!(!dir.kernel_io);
    assert!(fs.map_for_read(dir.ino, 0, 4096, 4).is_err());
    assert!(fs.map_for_write(dir.ino, 0, 4096, 4).is_err());
}
