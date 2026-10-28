use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(binary: &Path, root: &Path, args: &[&str]) {
    let status = Command::new(binary)
        .current_dir(root)
        .args(args)
        .status()
        .expect("run ext4 command");
    assert!(status.success(), "ext4 {:?} exited with {status}", args);
}

#[test]
fn offline_image_workflow_is_e2fsck_clean() {
    if !oracle_available("CLI image test", &[]) {
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let work = root
        .join("target")
        .join(format!("cli-image-workflow-{}", std::process::id()));
    let image = work.join("cli.img");
    let source = work.join("source");
    let extracted = work.join("extracted");
    fs::create_dir_all(source.join("nested")).expect("create source tree");
    fs::write(source.join("top.txt"), b"top-level data\n").expect("write top file");
    fs::write(source.join("nested/file.txt"), b"nested data\n").expect("write nested file");
    fs::File::create(&image)
        .expect("create image")
        .set_len(64 * 1024 * 1024)
        .expect("size image");

    let binary = PathBuf::from(env!("CARGO_BIN_EXE_ext4"));
    let image_arg = image.to_string_lossy();
    run(
        &binary,
        &root,
        &["mkfs", "-f", "-t", "ext4", "-L", "t", &image_arg],
    );
    run(
        &binary,
        &root,
        &["put", &image_arg, &source.to_string_lossy(), "/tree"],
    );
    run(&binary, &root, &["ls", "-l", &image_arg, "/tree"]);
    run(
        &binary,
        &root,
        &["get", &image_arg, "/tree", &extracted.to_string_lossy()],
    );

    let status = Command::new("diff")
        .args([
            "-r",
            source.as_os_str().to_str().unwrap(),
            extracted.as_os_str().to_str().unwrap(),
        ])
        .status()
        .expect("compare source and extracted trees");
    assert!(status.success(), "extracted tree differs from source");

    run(&binary, &root, &["rm", "-r", &image_arg, "/tree"]);
    let relative_image = image
        .strip_prefix(&root)
        .expect("image below repository root");
    let status = Command::new(root.join("tools/e2fs/run"))
        .current_dir(&root)
        .args(["e2fsck", "-fn", relative_image.to_str().unwrap()])
        .status()
        .expect("run e2fsck container");
    assert!(status.success(), "e2fsck reported an inconsistency");

    let _ = fs::remove_dir_all(work);
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
