mod support;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ext4_core::journal::{Journal, Run};
use ext4_core::{BlockDevice, FileDevice};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

fn oracle_available(test: &str) -> bool {
    support::oracle_or_skip(test)
}

fn e2fs(root: &Path, args: &[&str]) {
    let status = Command::new("tools/e2fs/run")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "tools/e2fs/run {args:?} failed");
}

fn e2fsck(root: &Path, image: &str) {
    let status = Command::new("tools/e2fs/run")
        .args(["e2fsck", "-fy", image])
        .current_dir(root)
        .status()
        .unwrap();
    assert!(
        status.code().is_some_and(|code| code <= 1),
        "e2fsck {image} failed"
    );
}

fn command_output(root: &Path, args: &[&str]) -> String {
    let output = Command::new("tools/e2fs/run")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "tools/e2fs/run {args:?} failed");
    String::from_utf8(output.stdout).unwrap()
}

fn journal_runs(root: &Path, image: &str) -> Vec<Run> {
    for _ in 0..3 {
        let output = command_output(root, &["debugfs", "-R", "blocks <8>", image]);
        let blocks: Vec<u64> = output
            .split_whitespace()
            .map(|word| word.parse::<u64>().unwrap())
            .collect();
        if blocks.is_empty() {
            std::thread::yield_now();
            continue;
        }
        let mut runs: Vec<Run> = Vec::new();
        for (logical, physical) in blocks.into_iter().enumerate() {
            match runs.last_mut() {
                Some(run) if run.physical + run.len == physical => run.len += 1,
                _ => runs.push(Run {
                    logical: logical as u64,
                    physical,
                    len: 1,
                }),
            }
        }
        return runs;
    }
    panic!("debugfs did not return journal blocks");
}

fn ext_geometry(image: &Path) -> (u32, [u8; 16]) {
    let bytes = fs::read(image).unwrap();
    let superblock = &bytes[1024..2048];
    let log_block_size = u32::from_le_bytes(superblock[24..28].try_into().unwrap());
    let block_size = 1024u32.checked_shl(log_block_size).unwrap();
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&superblock[104..120]);
    (block_size, uuid)
}

fn journal(root: &Path, image_name: &str) -> (FileDevice, Journal, u32) {
    let image = root.join(image_name);
    let (block_size, uuid) = ext_geometry(&image);
    let runs = journal_runs(root, image_name);
    let dev = FileDevice::open(&image, false).unwrap();
    let journal = Journal::open(&dev, block_size, runs, uuid).unwrap();
    (dev, journal, block_size)
}

fn create_image(root: &Path, name: &str, fs_type: &str, extra: &[&str]) {
    let image = format!("target/img/{name}.img");
    let _ = fs::remove_file(root.join(&image));
    let mut args = vec!["mke2fs", "-q", "-t", fs_type, "-F"];
    args.extend_from_slice(extra);
    args.extend([image.as_str(), "16M"]);
    e2fs(root, &args);
}

fn debugfs_transaction(root: &Path, image: &str, block: u64) {
    let script = format!("target/img/{image}.cmd");
    fs::write(
        root.join(&script),
        format!(
            "journal_open\njournal_write -b {block} target/img/{image}.img\njournal_close\nquit\n"
        ),
    )
    .unwrap();
    let path = format!("target/img/{image}.img");
    e2fs(root, &["debugfs", "-w", "-f", &script, &path]);
}

fn read_block(dev: &dyn BlockDevice, block: u64, block_size: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; block_size as usize];
    dev.read_at(block * u64::from(block_size), &mut bytes)
        .unwrap();
    bytes
}

#[test]
fn scanner_matches_e2fsck_replay_for_supported_mke2fs_formats() {
    let root = repository_root();
    if !oracle_available("journal oracle") {
        return;
    }
    fs::create_dir_all(root.join("target/img")).unwrap();

    let variants: [(&str, &str, &[&str]); 5] = [
        ("journal-oracle-v3", "ext4", &["-b", "4096"]),
        (
            "journal-oracle-no-metadata-csum",
            "ext4",
            &["-b", "4096", "-O", "^metadata_csum"],
        ),
        (
            "journal-oracle-no-64bit",
            "ext4",
            &["-b", "4096", "-O", "^64bit"],
        ),
        ("journal-oracle-1k", "ext4", &["-b", "1024"]),
        ("journal-oracle-ext3", "ext3", &["-b", "1024"]),
    ];
    for (name, fs_type, options) in variants {
        create_image(&root, name, fs_type, options);
        debugfs_transaction(&root, name, 3);
        let image_name = format!("target/img/{name}.img");
        let (dev, journal, block_size) = journal(&root, &image_name);
        let recovery = journal.scan_recovery(&dev).unwrap();
        assert!(recovery.transactions >= 1, "{name}");
        assert!(recovery.blocks.contains_key(&3), "{name}");
        let replayed = journal.read_replay_blocks(&dev, &recovery).unwrap();

        let fsck_path = root.join(format!("target/img/{name}-fsck.img"));
        let ours_path = root.join(format!("target/img/{name}-ours.img"));
        fs::copy(root.join(&image_name), &fsck_path).unwrap();
        fs::copy(root.join(&image_name), &ours_path).unwrap();
        let fsck_name = format!("target/img/{name}-fsck.img");
        let ours_name = format!("target/img/{name}-ours.img");

        let (ours_block_size, ours_uuid) = ext_geometry(&ours_path);
        assert_eq!(ours_block_size, block_size);
        let ours_runs = journal_runs(&root, &ours_name);
        let ours_device = FileDevice::open(&ours_path, true).unwrap();
        let mut ours_journal =
            Journal::open(&ours_device, block_size, ours_runs, ours_uuid).unwrap();
        let ours_recovery = ours_journal.scan_recovery(&ours_device).unwrap();
        let ours_replayed = ours_journal
            .read_replay_blocks(&ours_device, &ours_recovery)
            .unwrap();
        assert_eq!(ours_replayed, replayed, "{name}");
        for (home, data) in &ours_replayed {
            ours_device
                .write_at(home * u64::from(block_size), data)
                .unwrap();
        }
        ours_device.flush().unwrap();
        ours_journal
            .finish_recovery(&ours_device, ours_recovery.next_sequence)
            .unwrap();

        e2fsck(&root, &fsck_name);
        e2fsck(&root, &ours_name);
        let fsck_device = FileDevice::open(&fsck_path, false).unwrap();
        let ours_device = FileDevice::open(&ours_path, false).unwrap();
        for home in recovery.blocks.keys() {
            assert_eq!(
                read_block(&fsck_device, *home, block_size),
                read_block(&ours_device, *home, block_size),
                "{name}, home block {home}"
            );
        }
    }
}

#[test]
fn e2fsck_replays_writer_commit_record() {
    let root = repository_root();
    if !oracle_available("journal oracle") {
        return;
    }
    fs::create_dir_all(root.join("target/img")).unwrap();
    let name = "journal-writer-oracle";
    create_image(&root, name, "ext4", &["-b", "4096"]);
    let image_name = format!("target/img/{name}.img");
    let image = root.join(&image_name);
    let (block_size, uuid) = ext_geometry(&image);
    let runs = journal_runs(&root, &image_name);
    let dev = FileDevice::open(&image, true).unwrap();
    let source = read_block(&dev, 3, block_size);
    let mut writer = Journal::open(&dev, block_size, runs.clone(), uuid).unwrap();
    writer
        .commit_without_checkpoint(&dev, &[(3, source.as_slice())])
        .unwrap();

    let reader = FileDevice::open(&image, false).unwrap();
    let journal = Journal::open(&reader, block_size, runs, uuid).unwrap();
    let replay = journal.scan_recovery(&reader).unwrap();
    assert_eq!(
        journal.read_replay_blocks(&reader, &replay).unwrap()[&3],
        source
    );
    e2fsck(&root, &image_name);
    let checked = FileDevice::open(&image, false).unwrap();
    assert_eq!(read_block(&checked, 3, block_size), source);
}
