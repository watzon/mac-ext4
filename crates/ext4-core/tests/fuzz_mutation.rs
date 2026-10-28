//! Deterministic corruption smoke test for parser and mount hardening.
//!
//! Seeds are retained under `target/img/fuzz-seeds` so a failing mutation can
//! be reproduced from the seed name and iteration reported by this test.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use ext4_core::check::{check, CheckOptions};
use ext4_core::inode::FileType;
use ext4_core::mkfs::{format, MkfsOptions, Revision};
use ext4_core::volume::probe;
use ext4_core::{FileDevice, Filesystem, MemDevice, MountOptions};
use support::*;

const WALK_BYTES_LIMIT: u64 = 8 << 20;
const WALK_ENTRIES_LIMIT: usize = 512;
const WALK_DEPTH_LIMIT: usize = 8;
const CASE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
struct Seed {
    name: String,
    data: Vec<u8>,
    metadata: Vec<(usize, usize)>,
}

fn le16(bytes: &[u8], offset: usize) -> u16 {
    bytes
        .get(offset..offset + 2)
        .map(|v| u16::from_le_bytes([v[0], v[1]]))
        .unwrap_or(0)
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    bytes
        .get(offset..offset + 4)
        .map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
        .unwrap_or(0)
}

fn block_size(data: &[u8]) -> usize {
    let log = le32(data, 1024 + 0x18);
    1024usize
        .checked_shl(log)
        .filter(|n| *n <= 65536)
        .unwrap_or(1024)
}

/// Metadata regions inferred only from the stable seed's superblock and GDT.
/// A small whole-image region remains in the pool so directory data, extent
/// nodes, journal blocks, and external xattrs are also mutated.
fn metadata_regions(data: &[u8]) -> Vec<(usize, usize)> {
    let bs = block_size(data);
    let first = le32(data, 1024 + 0x14) as u64;
    let blocks = le32(data, 1024 + 0x04) as u64;
    let per_group = le32(data, 1024 + 0x20) as u64;
    let groups = if per_group == 0 {
        0
    } else {
        (blocks.saturating_sub(first)).div_ceil(per_group)
    };
    let desc_size = usize::from(le16(data, 1024 + 0xfe)).clamp(32, 1024);
    let gdt = if bs == 1024 { bs * 2 } else { bs };
    let mut regions = vec![(1024, 1024), (gdt, bs.min(data.len().saturating_sub(gdt)))];
    for group in 0..groups.min(64) as usize {
        let offset = match gdt.checked_add(group.saturating_mul(desc_size)) {
            Some(offset) if offset + 12 <= data.len() => offset,
            _ => break,
        };
        for block in [
            le32(data, offset),
            le32(data, offset + 4),
            le32(data, offset + 8),
        ] {
            let start = block as usize * bs;
            if start < data.len() {
                let len = if block == le32(data, offset + 8) {
                    bs.saturating_mul(12)
                } else {
                    bs
                };
                regions.push((start, len.min(data.len() - start)));
            }
        }
    }
    let early = data.len().min(bs.saturating_mul(256));
    if early > 0 {
        regions.push((0, early));
    }
    let body = data.len().saturating_sub(1024);
    if body > 0 {
        regions.push((1024, body));
    }
    regions
}

fn create_seed(
    root: &Path,
    seed_dir: &Path,
    name: &str,
    size: &str,
    args: &[&str],
    htree: bool,
) -> PathBuf {
    let target = seed_dir.join(format!("{name}.img"));
    if target.exists() {
        return target;
    }
    let (temporary, rel) = mke2fs(&format!("fuzz-seed-{name}"), size, args);
    let payload = root.join("target/img/fuzz-seed-payload");
    fs::write(&payload, b"mutation fuzz seed payload\n").expect("write fuzz payload");
    let mut script = String::from(
        "mkdir dirs\nmkdir dirs/nested\nwrite target/img/fuzz-seed-payload file\nwrite target/img/fuzz-seed-payload dirs/nested/file\nsymlink file link\nea_set file user.fuzz seed-value\n",
    );
    if htree {
        script.push_str("mkdir indexed\n");
        for entry in 0..2_000 {
            script.push_str(&format!(
                "write target/img/fuzz-seed-payload indexed/item-{entry:03}\n"
            ));
        }
    }
    let debugfs = debugfs_w(&rel, &script);
    assert!(debugfs.status.success(), "{}", text(&debugfs));
    if htree {
        let optimized = e2fs(&["e2fsck", "-fyD", &rel]);
        assert!(
            optimized.status.code().is_some_and(|code| code <= 1),
            "{}",
            text(&optimized)
        );
    }
    fs::copy(temporary, &target).expect("cache fuzz seed");
    target
}

fn create_own_seed(root: &Path, seed_dir: &Path) -> PathBuf {
    let target = seed_dir.join("our-ext4-4k.img");
    if target.exists() {
        return target;
    }
    let size = 8 * 1024 * 1024;
    let file = fs::File::create(&target).expect("create own fuzz seed");
    file.set_len(size as u64).expect("size own fuzz seed");
    drop(file);
    let dev = FileDevice::open(&target, true).expect("open own fuzz seed");
    let mut progress = |_| {};
    format(
        &dev,
        &MkfsOptions {
            label: "fuzz-own".into(),
            revision: Revision::Ext4,
            ..MkfsOptions::default()
        },
        &mut progress,
    )
    .expect("format own fuzz seed");
    drop(dev);
    let rel = target
        .strip_prefix(root)
        .expect("seed below workspace")
        .to_str()
        .expect("utf-8 seed path");
    let payload = root.join("target/img/fuzz-seed-payload");
    fs::write(&payload, b"our mutation fuzz seed payload\n").expect("write fuzz payload");
    let debugfs = debugfs_w(rel, "write target/img/fuzz-seed-payload file\nsymlink file link\nea_set file user.fuzz seed-value\n");
    assert!(debugfs.status.success(), "{}", text(&debugfs));
    target
}

fn seeds() -> Vec<Seed> {
    let root = workspace();
    let seed_dir = root.join("target/img/fuzz-seeds");
    fs::create_dir_all(&seed_dir).expect("create fuzz seed directory");
    let specs: [(&str, &str, &[&str], bool); 5] = [
        (
            "ext2-1k",
            "8M",
            &["-t", "ext2", "-b", "1024", "-I", "256"],
            false,
        ),
        (
            "ext3-4k",
            "8M",
            &["-t", "ext3", "-b", "4096", "-I", "256"],
            false,
        ),
        (
            "ext4-1k",
            "8M",
            &["-t", "ext4", "-b", "1024", "-I", "256"],
            false,
        ),
        (
            "ext4-4k-htree",
            "16M",
            &["-t", "ext4", "-b", "4096", "-I", "256"],
            true,
        ),
        (
            "ext4-inline",
            "8M",
            &["-t", "ext4", "-b", "4096", "-I", "256", "-O", "inline_data"],
            false,
        ),
    ];
    let mut paths: Vec<(String, PathBuf)> = specs
        .iter()
        .map(|(name, size, args, htree)| {
            (
                (*name).to_owned(),
                create_seed(&root, &seed_dir, name, size, args, *htree),
            )
        })
        .collect();
    paths.push(("our-ext4-4k".into(), create_own_seed(&root, &seed_dir)));
    paths
        .into_iter()
        .map(|(name, path)| {
            let data = fs::read(path).expect("read cached fuzz seed");
            let metadata = metadata_regions(&data);
            Seed {
                name,
                data,
                metadata,
            }
        })
        .collect()
}

fn random(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

fn mutate(seed: &Seed, iteration: usize) -> Vec<u8> {
    let mut image = seed.data.clone();
    let mut state =
        0xD1CE_CAFE_5EED_1234u64 ^ iteration as u64 ^ (seed.name.len() as u64).rotate_left(29);
    let flips = 1 + (random(&mut state) % 8) as usize;
    for _ in 0..flips {
        let &(start, len) = &seed.metadata[(random(&mut state) as usize) % seed.metadata.len()];
        if len == 0 {
            continue;
        }
        let offset = start + (random(&mut state) as usize % len);
        if let Some(byte) = image.get_mut(offset) {
            *byte ^= 1 << (random(&mut state) % 8);
        }
    }
    image
}

fn walk(fs: &Filesystem, ino: u32, depth: usize, seen: &mut usize) {
    if depth > WALK_DEPTH_LIMIT || *seen >= WALK_ENTRIES_LIMIT {
        return;
    }
    let Ok(attr) = fs.getattr(ino) else { return };
    *seen += 1;
    let Ok(names) = fs.list_xattrs(ino) else {
        return;
    };
    for name in names.into_iter().take(32) {
        let _ = fs.get_xattr(ino, &name);
    }
    match attr.file_type {
        FileType::Directory if attr.size <= WALK_BYTES_LIMIT => {
            let mut entries = Vec::new();
            if fs
                .read_dir(ino, 0, |entry, _| {
                    if entries.len() < WALK_ENTRIES_LIMIT {
                        entries.push(entry);
                    }
                    entries.len() < WALK_ENTRIES_LIMIT
                })
                .is_ok()
            {
                for entry in entries {
                    if entry.name != b"." && entry.name != b".." {
                        walk(fs, entry.inode, depth + 1, seen);
                    }
                }
            }
        }
        FileType::Symlink => {
            let _ = fs.readlink(ino);
        }
        _ if attr.size <= WALK_BYTES_LIMIT => {
            let mut offset = 0;
            let mut buf = [0u8; 8192];
            while offset < attr.size {
                let n = (attr.size - offset).min(buf.len() as u64) as usize;
                match fs.read(ino, offset, &mut buf[..n]) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => offset += read as u64,
                }
            }
        }
        _ => {}
    }
}

fn run_case(image: Vec<u8>) {
    let _ = probe(&MemDevice::from_vec(image.clone()));
    if let Ok(fs) = Filesystem::open(
        Box::new(MemDevice::from_vec(image.clone())),
        &MountOptions {
            read_only: true,
            ignore_checksums: false,
        },
    ) {
        let mut seen = 0;
        walk(&fs, 2, 0, &mut seen);
        let _ = fs.unmount();
    }
    fn discard_message(_: &str) {}
    let mut message = discard_message;
    let mut progress = |_: f64| {};
    let _ = check(
        Box::new(MemDevice::from_vec(image.clone())),
        &CheckOptions {
            quick: true,
            repair: false,
            force: false,
        },
        &mut message,
        &mut progress,
    );
    let _ = check(
        Box::new(MemDevice::from_vec(image.clone())),
        &CheckOptions::default(),
        &mut message,
        &mut progress,
    );
    if let Ok(fs) = Filesystem::open(
        Box::new(MemDevice::from_vec(image)),
        &MountOptions {
            read_only: false,
            ignore_checksums: false,
        },
    ) {
        if let Ok(created) = fs.create(2, b"fuzz-write", 0o100600, 0, 0, (0, 0)) {
            let _ = fs.write(created.ino, 0, b"fuzz");
            let _ = fs.remove(2, b"fuzz-write");
        }
        let _ = fs.unmount();
    }
}

fn save_failure(root: &Path, seed: &str, iteration: usize, image: &[u8]) -> PathBuf {
    let dir = root.join("target/img/fuzz-failures");
    fs::create_dir_all(&dir).expect("create fuzz failure directory");
    let path = dir.join(format!("{seed}-{iteration:04}.img"));
    fs::write(&path, image).expect("save fuzz failure image");
    path
}

#[test]
fn corrupted_metadata_never_panics_or_hangs() {
    if !oracle_or_skip("mutation fuzz seeds") {
        return;
    }
    let root = workspace();
    let seeds = seeds();
    let iterations = std::env::var("EXT4_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000usize);
    for iteration in 0..iterations {
        let seed = &seeds[iteration % seeds.len()];
        let image = mutate(seed, iteration);
        let saved = image.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(image)));
            let _ = sender.send(result.map_err(|_| "panic"));
        });
        match receiver.recv_timeout(CASE_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => {
                let path = save_failure(&root, &seed.name, iteration, &saved);
                panic!("fuzz {reason} in seed {} iteration {iteration}; reproduce with EXT4_FUZZ_ITERS={} and inspect {}", seed.name, iteration + 1, path.display());
            }
            Err(_) => {
                let path = save_failure(&root, &seed.name, iteration, &saved);
                panic!(
                    "fuzz timeout in seed {} iteration {iteration}; image saved at {}",
                    seed.name,
                    path.display()
                );
            }
        }
    }
}
