//! Shared helpers for oracle tests that run e2fsprogs in Docker.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ext4_core::{FileDevice, Filesystem, MountOptions};

pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// True if the e2fsprogs oracle (`tools/e2fs/run`) can run `tools`
/// (default: mke2fs, e2fsck, debugfs, dumpe2fs). Otherwise the test is
/// skipped with a note, or fails when EXT4_REQUIRE_ORACLE is set (CI).
pub fn oracle_available(test: &str, tools: &[&str]) -> bool {
    let ok = Command::new(workspace().join("tools/e2fs/run"))
        .arg("--check")
        .args(tools)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        assert!(
            std::env::var_os("EXT4_REQUIRE_ORACLE").is_none(),
            "{test}: EXT4_REQUIRE_ORACLE is set, but tools/e2fs/run cannot run {tools:?} (start Docker, or brew install e2fsprogs gptfdisk)"
        );
        eprintln!("skipping {test}: e2fsprogs is not available (start Docker, or brew install e2fsprogs gptfdisk)");
    }
    ok
}

pub fn oracle_or_skip(test: &str) -> bool {
    oracle_available(test, &[])
}

/// Run an e2fsprogs tool. Paths in `args` are repository-relative.
pub fn e2fs(args: &[&str]) -> Output {
    Command::new("tools/e2fs/run")
        .args(args)
        .current_dir(workspace())
        .output()
        .expect("run e2fsprogs container")
}

pub fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// Create `target/img/<name>.img` of `size` bytes with mke2fs `args`.
/// Returns (absolute path, repository-relative path).
pub fn mke2fs(name: &str, size: &str, args: &[&str]) -> (PathBuf, String) {
    let rel = format!("target/img/{name}.img");
    let abs = workspace().join(&rel);
    fs::create_dir_all(abs.parent().unwrap()).unwrap();
    let _ = fs::remove_file(&abs);
    let mut full = vec!["mke2fs", "-q", "-F"];
    full.extend_from_slice(args);
    full.push(&rel);
    full.push(size);
    let o = e2fs(&full);
    assert!(o.status.success(), "mke2fs failed: {}", text(&o));
    (abs, rel)
}

/// Run debugfs -w with a script of commands (one per line).
pub fn debugfs_w(rel: &str, script: &str) -> Output {
    let script_rel = format!("{rel}.cmds");
    fs::write(workspace().join(&script_rel), script).unwrap();
    let o = e2fs(&["debugfs", "-w", "-f", &script_rel, rel]);
    let _ = fs::remove_file(workspace().join(&script_rel));
    o
}

pub fn debugfs_r(rel: &str, cmd: &str) -> String {
    text(&e2fs(&["debugfs", "-R", cmd, rel]))
}

/// Assert `e2fsck -fn` reports a clean file system.
pub fn assert_fsck_clean(rel: &str) {
    let o = e2fs(&["e2fsck", "-fn", rel]);
    assert!(
        o.status.success(),
        "e2fsck -fn {rel} reported problems:\n{}",
        text(&o)
    );
}

pub fn mount(abs: &Path, read_only: bool) -> Filesystem {
    let dev = FileDevice::open(abs, !read_only).expect("open image");
    Filesystem::open(
        Box::new(dev),
        &MountOptions {
            read_only,
            ..Default::default()
        },
    )
    .expect("mount")
}

/// Resolve a path from the root, following no symlinks.
pub fn resolve(fs: &Filesystem, path: &str) -> u32 {
    let mut ino = 2;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        ino = fs
            .lookup(ino, part.as_bytes())
            .unwrap_or_else(|e| panic!("lookup {part} in {path}: {e}"))
            .ino;
    }
    ino
}

pub fn read_all(fs: &Filesystem, ino: u32) -> Vec<u8> {
    let size = fs.getattr(ino).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

/// Deterministic pseudo-random bytes.
pub fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}
