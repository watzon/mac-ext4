//! `ext4 set-type` must change only the type GUID of one entry and leave a
//! GPT that gdisk accepts.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn oracle(args: &[&str]) -> Output {
    Command::new("tools/e2fs/run")
        .args(args)
        .current_dir(root())
        .output()
        .expect("run oracle")
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn set_type_rewrites_both_tables_and_keeps_other_fields() {
    if !oracle_available("set-type test", &["sgdisk"]) {
        return;
    }
    let rel = format!("target/img/set-type-{}.img", std::process::id());
    let image = root().join(&rel);
    fs::create_dir_all(image.parent().unwrap()).unwrap();
    fs::File::create(&image).unwrap().set_len(64 << 20).unwrap();
    // Two partitions, the second typed Microsoft Basic Data and named.
    let o = oracle(&[
        "sgdisk",
        "-n",
        "1:2048:+8M",
        "-t",
        "1:EF00",
        "-n",
        "2:0:0",
        "-t",
        "2:0700",
        "-c",
        "2:SHARED",
        &rel,
    ]);
    assert!(o.status.success(), "{}", text(&o));
    let before = text(&oracle(&["sgdisk", "-i", "2", &rel]));

    let binary = PathBuf::from(env!("CARGO_BIN_EXE_ext4"));
    let o = Command::new(&binary)
        .args(["set-type", "--yes", "--index", "2", image.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o));

    let verify = oracle(&["sgdisk", "-v", &rel]);
    assert!(
        text(&verify).contains("No problems found"),
        "{}",
        text(&verify)
    );
    let after = text(&oracle(&["sgdisk", "-i", "2", &rel]));
    assert!(
        after.contains("0FC63DAF-8483-4772-8E79-3D69D8477DE4 (Linux filesystem)"),
        "{after}"
    );
    // Everything except the type line is unchanged.
    let strip = |s: &str| {
        s.lines()
            .filter(|l| !l.starts_with("Partition GUID code"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip(&before), strip(&after));
    let first = text(&oracle(&["sgdisk", "-i", "1", &rel]));
    assert!(first.contains("EFI system partition"), "{first}");

    // A missing entry is an error, and nothing changes.
    let o = Command::new(&binary)
        .args(["set-type", "--yes", "--index", "5", image.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!o.status.success());
    let _ = fs::remove_file(&image);
}

/// Raw disk devices accept only whole-sector transfers; run the command on
/// an attached disk image, which behaves like a physical disk.
#[cfg(target_os = "macos")]
#[test]
fn set_type_works_on_a_raw_disk_device() {
    if !oracle_available("raw set-type test", &["sgdisk"]) {
        return;
    }
    let rel = format!("target/img/set-type-raw-{}.img", std::process::id());
    let image = root().join(&rel);
    fs::create_dir_all(image.parent().unwrap()).unwrap();
    fs::File::create(&image).unwrap().set_len(64 << 20).unwrap();
    let o = oracle(&["sgdisk", "-n", "1:2048:0", "-t", "1:0700", &rel]);
    assert!(o.status.success(), "{}", text(&o));
    let attach = Command::new("diskutil")
        .args(["image", "attach", "--noMount"])
        .arg(&image)
        .output()
        .unwrap();
    assert!(attach.status.success(), "{}", text(&attach));
    let dev = String::from_utf8_lossy(&attach.stdout)
        .split_whitespace()
        .find(|w| w.starts_with("/dev/disk"))
        .map(|w| w.trim_start_matches("/dev/").to_string())
        .expect("attached device");
    // "disk10" or "disk10s1" -> "disk10"
    let digits: String = dev["disk".len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let base = format!("disk{digits}");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_ext4"));
    let o = Command::new(&binary)
        .args(["set-type", "--yes", &format!("{base}s1")])
        .output()
        .unwrap();
    let _ = Command::new("diskutil").args(["eject", &base]).output();
    assert!(o.status.success(), "{}", text(&o));
    let after = text(&oracle(&["sgdisk", "-i", "1", &rel]));
    assert!(after.contains("(Linux filesystem)"), "{after}");
    assert!(text(&oracle(&["sgdisk", "-v", &rel])).contains("No problems found"));
    let _ = fs::remove_file(&image);
}

/// True if tools/e2fs/run can run `tools`. Skips with a note, or fails when
/// EXT4_REQUIRE_ORACLE is set (CI).
fn oracle_available(test: &str, tools: &[&str]) -> bool {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ok = std::process::Command::new(root.join("tools/e2fs/run"))
        .arg("--check")
        .args(tools)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        assert!(
            std::env::var_os("EXT4_REQUIRE_ORACLE").is_none(),
            "{test}: EXT4_REQUIRE_ORACLE is set, but tools/e2fs/run cannot run {tools:?}"
        );
        eprintln!("skipping {test}: e2fsprogs is not available (start Docker, or brew install e2fsprogs gptfdisk)");
    }
    ok
}
