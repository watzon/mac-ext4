use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};

mod da;
mod gpt;
use ext4_core::check::{check, CheckOptions};
use ext4_core::inode::{FileType, S_IFREG};
use ext4_core::mkfs::{format, MkfsOptions, Revision};
use ext4_core::{probe, Attr, FileDevice, Filesystem, MountOptions};

const ROOT_INO: u32 = 2;
const COPY_BUFFER_SIZE: usize = 1024 * 1024;

type CliResult<T> = Result<T, Box<dyn Error>>;

#[derive(Parser)]
#[command(
    name = "ext4",
    about = "Offline ext2/ext3/ext4 utility and FSKit mount frontend"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandName,
}

#[derive(Subcommand)]
enum CommandName {
    /// Find ext-family partitions reported by Disk Utility.
    List,
    /// Print filesystem metadata.
    Info { dev: String },
    /// Create an ext2, ext3, or ext4 filesystem.
    Mkfs {
        #[arg(short = 't', default_value = "ext4", value_parser = ["ext2", "ext3", "ext4"])]
        filesystem_type: String,
        #[arg(short = 'L')]
        label: Option<String>,
        #[arg(short = 'b')]
        block_size: Option<u32>,
        #[arg(short = 'I')]
        inode_size: Option<u16>,
        #[arg(short = 'i')]
        bytes_per_inode: Option<u32>,
        #[arg(short = 'm')]
        reserved_percent: Option<u32>,
        /// Skip mounted-device and whole-disk protection, plus confirmation.
        #[arg(short = 'f', long)]
        force: bool,
        /// Skip only the interactive confirmation.
        #[arg(long)]
        yes: bool,
        dev: String,
    },
    /// Check a filesystem. Exit status follows fsck conventions.
    Check {
        #[arg(short = 'n', conflicts_with = "repair")]
        read_only: bool,
        #[arg(short = 'y', long)]
        repair: bool,
        #[arg(short = 'q', long)]
        quick: bool,
        dev: String,
    },
    /// Mount through the installed FSKit extension.
    Mount {
        #[arg(short = 'r', long)]
        read_only: bool,
        /// Mount every unmounted partition of type "Linux filesystem".
        /// At login this mounts the disks that macOS could not mount at
        /// startup, before any user had the FSKit module enabled.
        #[arg(long, conflicts_with_all = ["disk", "mountpoint"])]
        all: bool,
        #[arg(required_unless_present = "all")]
        disk: Option<String>,
        mountpoint: Option<PathBuf>,
    },
    /// Unmount a volume by disk name or mount point (no sudo needed).
    Unmount {
        /// Unmount even if files are open.
        #[arg(short = 'f', long)]
        force: bool,
        target: String,
    },
    /// List a directory without mounting through FSKit.
    Ls {
        #[arg(short = 'l')]
        long: bool,
        dev: String,
        path: Option<String>,
    },
    /// Write one regular file to standard output.
    Cat { dev: String, path: String },
    /// Copy an item to the host. Directories are copied recursively.
    Get {
        dev: String,
        path: String,
        host_dest: PathBuf,
    },
    /// Copy a host item into the filesystem. Directories are copied recursively.
    Put {
        dev: String,
        host_src: PathBuf,
        path: String,
    },
    /// Create a directory.
    Mkdir { dev: String, path: String },
    /// Remove an item, recursively with -r.
    Rm {
        #[arg(short = 'r', long)]
        recursive: bool,
        dev: String,
        path: String,
    },
    /// Rename an item.
    Mv {
        dev: String,
        source: String,
        destination: String,
    },
    /// Create a symbolic link. `-s` is required.
    Ln {
        #[arg(short = 's')]
        symbolic: bool,
        dev: String,
        target: String,
        path: String,
    },
    /// Print attributes for an item.
    Stat { dev: String, path: String },
    /// Set the GPT partition type (default: Linux filesystem). Takes a
    /// partition such as disk8s2, or an image file with --index.
    SetType {
        target: String,
        /// `linux` or a type GUID.
        #[arg(short = 't', long = "type", default_value = "linux")]
        type_name: String,
        /// Partition number, required for image files.
        #[arg(short = 'i', long)]
        index: Option<u32>,
        /// Do not ask for confirmation.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

struct MkfsArguments {
    filesystem_type: String,
    label: Option<String>,
    block_size: Option<u32>,
    inode_size: Option<u16>,
    inode_ratio: Option<u32>,
    reserved_percent: Option<u32>,
    force: bool,
    yes: bool,
}

fn main() -> ExitCode {
    // Disk Utility formats through /Library/Filesystems/ext4.fs, whose
    // FSFormatExecutable is this binary installed as `newfs_ext4`.
    let argv0 = std::env::args_os().next().unwrap_or_default();
    let program = Path::new(&argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if let Some(kind) = program.strip_prefix("newfs_") {
        return match newfs(kind, std::env::args().skip(1).collect()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{program}: {error}");
                ExitCode::from(1)
            }
        };
    }
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ext4: {error}");
            ExitCode::from(8)
        }
    }
}

fn run(command: CommandName) -> CliResult<()> {
    match command {
        CommandName::List => list(),
        CommandName::SetType {
            target,
            type_name,
            index,
            yes,
        } => set_type(&target, &type_name, index, yes),
        CommandName::Info { dev } => info(&dev),
        CommandName::Mkfs {
            filesystem_type,
            label,
            block_size,
            inode_size,
            bytes_per_inode,
            reserved_percent,
            force,
            yes,
            dev,
        } => mkfs(
            &dev,
            MkfsArguments {
                filesystem_type,
                label,
                block_size,
                inode_size,
                inode_ratio: bytes_per_inode,
                reserved_percent,
                force,
                yes,
            },
        ),
        CommandName::Check {
            read_only: _,
            repair,
            quick,
            dev,
        } => check_command(&dev, repair, quick),
        CommandName::Mount {
            read_only,
            all: true,
            ..
        } => mount_all(read_only),
        CommandName::Mount {
            read_only,
            disk,
            mountpoint,
            ..
        } => mount(
            disk.as_deref().expect("clap requires a disk without --all"),
            mountpoint.as_deref(),
            read_only,
        ),
        CommandName::Unmount { force, target } => unmount(&target, force),
        CommandName::Ls { long, dev, path } => with_filesystem(&dev, false, |fs| {
            ls(fs, path.as_deref().unwrap_or("/"), long)
        }),
        CommandName::Cat { dev, path } => with_filesystem(&dev, false, |fs| cat(fs, &path)),
        CommandName::Get {
            dev,
            path,
            host_dest,
        } => with_filesystem(&dev, false, |fs| get(fs, &path, &host_dest)),
        CommandName::Put {
            dev,
            host_src,
            path,
        } => with_filesystem(&dev, true, |fs| put(fs, &host_src, &path)),
        CommandName::Mkdir { dev, path } => with_filesystem(&dev, true, |fs| mkdir(fs, &path)),
        CommandName::Rm {
            recursive,
            dev,
            path,
        } => with_filesystem(&dev, true, |fs| remove(fs, &path, recursive)),
        CommandName::Mv {
            dev,
            source,
            destination,
        } => with_filesystem(&dev, true, |fs| rename(fs, &source, &destination)),
        CommandName::Ln {
            symbolic,
            dev,
            target,
            path,
        } => {
            if !symbolic {
                return Err("only symbolic links are supported; pass -s".into());
            }
            with_filesystem(&dev, true, |fs| symlink(fs, &target, &path))
        }
        CommandName::Stat { dev, path } => with_filesystem(&dev, false, |fs| stat(fs, &path)),
    }
}

fn open_device(dev: &str, writable: bool) -> CliResult<FileDevice> {
    Ok(FileDevice::open(device_path(dev), writable)?)
}

fn device_path(dev: &str) -> PathBuf {
    if dev.starts_with("/dev/") {
        let tail = dev.trim_start_matches("/dev/");
        if let Some(raw) = tail.strip_prefix("disk") {
            return PathBuf::from(format!("/dev/rdisk{raw}"));
        }
        return PathBuf::from(dev);
    }
    if dev.starts_with("disk") {
        return PathBuf::from(format!("/dev/r{dev}"));
    }
    PathBuf::from(dev)
}

fn with_filesystem<T>(
    dev: &str,
    writable: bool,
    operation: impl FnOnce(&Filesystem) -> CliResult<T>,
) -> CliResult<T> {
    let device = open_device(dev, writable)?;
    let fs = Filesystem::open(Box::new(device), &MountOptions::default())?;
    let result = operation(&fs);
    let unmount_result = fs.unmount();
    match (result, unmount_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(Box::new(error)),
        (Err(error), _) => Err(error),
    }
}

fn list() -> CliResult<()> {
    if !cfg!(target_os = "macos") {
        return Err("`ext4 list` requires macOS diskutil".into());
    }
    let output = Command::new("diskutil").args(["list", "-plist"]).output()?;
    if !output.status.success() {
        return Err("diskutil list failed".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for name in plist_values(&text, "DeviceIdentifier") {
        match open_device(&name, false).and_then(|device| probe(&device).map_err(Into::into)) {
            Ok(info) => println!(
                "{name}\t{}\t{}\t{} bytes\t{}",
                info.label,
                info.flavor,
                info.blocks * u64::from(info.block_size),
                if info.writable {
                    "writable"
                } else {
                    "read-only"
                }
            ),
            Err(error) if raw_access_denied(&error) => eprintln!(
                "{name}: permission denied; rerun `ext4 list` with sudo to probe raw devices"
            ),
            Err(_) => {}
        }
    }
    Ok(())
}

fn info(dev: &str) -> CliResult<()> {
    let device = open_device(dev, false)?;
    let probe_info = probe(&device)?;
    let fs = Filesystem::open(
        Box::new(device),
        &MountOptions {
            read_only: true,
            ignore_checksums: false,
        },
    )?;
    let stat = fs.statfs();
    let (compat, incompat, ro_compat, state, journal_inode) = fs.with_volume(|volume| {
        (
            volume.sb.feature_compat(),
            volume.sb.feature_incompat(),
            volume.sb.feature_ro_compat(),
            volume.sb.state(),
            volume.sb.journal_inum(),
        )
    });
    println!("label: {}", probe_info.label);
    println!("flavor: {}", probe_info.flavor);
    println!("uuid: {}", hex(&stat.uuid));
    println!("block size: {}", stat.block_size);
    println!("blocks: {}", stat.blocks);
    println!("free blocks: {}", stat.free_blocks);
    println!("available blocks: {}", stat.avail_blocks);
    println!("inodes: {} free: {}", stat.inodes, stat.free_inodes);
    println!("read-only: {}", stat.read_only);
    println!("features: compat={compat:#x} incompat={incompat:#x} ro_compat={ro_compat:#x}");
    println!(
        "journal: {}",
        if journal_inode == 0 {
            "none".to_owned()
        } else {
            format!("inode {journal_inode}, state {state:#x}")
        }
    );
    fs.unmount()?;
    Ok(())
}

/// `newfs_<type>` command-line convention, as used by Disk Utility and
/// `diskutil eraseVolume`: options, then the device as the last argument.
///   -v NAME   volume name (also -L)
///   -t TYPE   ext2, ext3, or ext4 (default from the program name)
///   -b SIZE, -I SIZE, -i BYTES, -m PERCENT  as in `ext4 mkfs`
/// Other single-letter flags are accepted and ignored, so that arguments
/// meant for other formatters do not make the erase fail.
fn newfs(kind: &str, args: Vec<String>) -> CliResult<()> {
    let mut filesystem_type = match kind {
        "ext2" | "ext3" | "ext4" => kind.to_string(),
        _ => "ext4".to_string(),
    };
    let mut label: Option<String> = None;
    let mut block_size = None;
    let mut inode_size = None;
    let mut inode_ratio = None;
    let mut reserved_percent = None;
    let mut device = None;
    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-v" | "-L" => label = Some(value(&arg)?),
            "-t" => filesystem_type = value("-t")?,
            "-b" => block_size = Some(value("-b")?.parse()?),
            "-I" => inode_size = Some(value("-I")?.parse()?),
            "-i" => inode_ratio = Some(value("-i")?.parse()?),
            "-m" => reserved_percent = Some(value("-m")?.parse()?),
            a if a.starts_with('-') && a.len() == 2 => {}
            _ => device = Some(arg),
        }
    }
    let device = device.ok_or("usage: newfs_ext4 [-v name] [-t ext2|ext3|ext4] device")?;
    // ext labels hold 16 bytes; Disk Utility allows longer names.
    let label = label.map(|l| {
        let mut end = l.len().min(16);
        while !l.is_char_boundary(end) {
            end -= 1;
        }
        l[..end].to_string()
    });
    mkfs(
        &device,
        MkfsArguments {
            filesystem_type,
            label,
            block_size,
            inode_size,
            inode_ratio,
            reserved_percent,
            force: true,
            yes: true,
        },
    )
}

fn mkfs(dev: &str, args: MkfsArguments) -> CliResult<()> {
    if !args.force && is_mounted(dev)? {
        return Err("refusing to format a mounted device; unmount it or use -f".into());
    }
    if !args.force && is_whole_disk_with_partitions(dev)? {
        return Err(
            "refusing to format a whole disk with partitions; select a partition or use -f".into(),
        );
    }
    if !args.force && !args.yes {
        eprint!(
            "Format {}? This permanently erases its contents [y/N] ",
            dev
        );
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            return Err("format cancelled".into());
        }
    }
    let revision = match args.filesystem_type.as_str() {
        "ext2" => Revision::Ext2,
        "ext3" => Revision::Ext3,
        "ext4" => Revision::Ext4,
        _ => return Err("filesystem type must be ext2, ext3, or ext4".into()),
    };
    let options = MkfsOptions {
        label: args.label.unwrap_or_default(),
        revision,
        block_size: args.block_size,
        inode_size: args.inode_size,
        inode_ratio: args.inode_ratio,
        reserved_percent: args.reserved_percent,
        ..Default::default()
    };
    let device = open_device(dev, true)?;
    let mut last = 0_u8;
    let report = format(&device, &options, &mut |fraction| {
        let percent = (fraction.clamp(0.0, 1.0) * 100.0) as u8;
        if percent != last {
            eprint!("\rFormatting: {percent:3}%");
            last = percent;
        }
    })?;
    eprintln!("\rFormatting: 100%");
    println!(
        "created {}: {} blocks, {} inodes, {} groups",
        args.filesystem_type, report.blocks, report.inodes, report.groups
    );
    Ok(())
}

fn check_command(dev: &str, repair: bool, quick: bool) -> CliResult<()> {
    let device = open_device(dev, repair)?;
    let mut messages = Vec::new();
    let mut last = 0_u8;
    let report = check(
        Box::new(device),
        &CheckOptions {
            repair,
            quick,
            force: true,
        },
        &mut |message| messages.push(message.to_owned()),
        &mut |fraction| {
            let percent = (fraction.clamp(0.0, 1.0) * 100.0) as u8;
            if percent != last {
                eprint!("\rChecking: {percent:3}%");
                last = percent;
            }
        },
    )?;
    eprintln!("\rChecking: 100%");
    for message in messages {
        eprintln!("{message}");
    }
    if report.clean {
        return Ok(());
    }
    if repair && !report.repaired.is_empty() {
        std::process::exit(1);
    }
    std::process::exit(4);
}

/// BSD name (`disk8s2`) for a `diskN…` or `/dev/[r]diskN…` argument.
fn bsd_name(disk: &str) -> String {
    let tail = disk.trim_start_matches("/dev/");
    tail.strip_prefix('r')
        .filter(|t| t.starts_with("disk"))
        .unwrap_or(tail)
        .to_string()
}

/// Mount point of a BSD device, from the system mount table.
fn mount_point_of(bsd: &str) -> Option<String> {
    let out = Command::new("/sbin/mount").output().ok()?;
    let dev = format!("/dev/{bsd} on ");
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        l.strip_prefix(&dev)
            .and_then(|rest| rest.rsplit_once(" ("))
            .map(|(m, _)| m.to_string())
    })
}

/// BSD device mounted at `path`, from the system mount table.
fn device_at(path: &Path) -> Option<String> {
    let out = Command::new("/sbin/mount").output().ok()?;
    let want = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let (dev, rest) = l.split_once(" on ")?;
        let (mnt, _) = rest.rsplit_once(" (")?;
        let mnt = Path::new(mnt);
        let same = mnt == want || mnt.canonicalize().ok().as_deref() == Some(want.as_path());
        (same && dev.starts_with("/dev/")).then(|| bsd_name(dev))
    })
}

const ENABLE_HINT: &str =
    "Confirm that Ext4 is enabled: System Settings > General > Login Items & Extensions, \
list 'By Category', (i) next to File System Extensions. Check registration with \
`pluginkit -m -p com.apple.fskit.fsmodule -v`.";

/// Mount each unmounted "Linux Filesystem" partition through Disk
/// Arbitration. Failures are reported and do not stop the others.
fn mount_all(read_only: bool) -> CliResult<()> {
    let output = Command::new("diskutil").args(["list", "-plist"]).output()?;
    if !output.status.success() {
        return Err("diskutil list failed".into());
    }
    let mut failed = 0;
    for name in plist_values(&String::from_utf8_lossy(&output.stdout), "DeviceIdentifier") {
        if split_partition(&name).is_none() {
            continue;
        }
        let info = Command::new("diskutil")
            .args(["info", "-plist", &name])
            .output()?;
        let text = String::from_utf8_lossy(&info.stdout);
        let linux = plist_values(&text, "Content")
            .iter()
            .any(|c| c == "Linux Filesystem");
        let mounted = plist_values(&text, "MountPoint")
            .iter()
            .any(|m| !m.is_empty());
        if linux && !mounted {
            if let Err(error) = mount(&name, None, read_only) {
                eprintln!("ext4: {name}: {error}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        return Err(format!("{failed} partition(s) did not mount").into());
    }
    Ok(())
}

fn mount(disk: &str, mountpoint: Option<&Path>, read_only: bool) -> CliResult<()> {
    let Some(mountpoint) = mountpoint else {
        // Disk Arbitration checks the volume, mounts it under /Volumes, and
        // lets the console user eject it later, all without sudo.
        let bsd = bsd_name(disk);
        if let Some(m) = mount_point_of(&bsd) {
            println!("already mounted at {m}");
            return Ok(());
        }
        // Right after an FSKit volume unmounts, Disk Arbitration's next probe
        // can fail while the old module instance is still shutting down, and
        // the disk is then reported "unsupported". A second request re-probes.
        let first = da::mount(&bsd, read_only);
        let result = match first {
            Err(e) if e.contains("0xf8da000c") => {
                std::thread::sleep(std::time::Duration::from_secs(2));
                da::mount(&bsd, read_only)
            }
            other => other,
        };
        result.map_err(|e| format!("mount failed: {e}. {ENABLE_HINT}"))?;
        match mount_point_of(&bsd) {
            Some(m) => println!("mounted at {m}"),
            None => println!("mounted"),
        }
        return Ok(());
    };
    fs::create_dir_all(mountpoint)?;
    let mut args = vec![
        OsString::from("-F"),
        OsString::from("-t"),
        OsString::from("ext4"),
    ];
    if read_only {
        args.extend([OsString::from("-o"), OsString::from("rdonly")]);
    }
    args.extend([
        OsString::from(bsd_name(disk)),
        mountpoint.as_os_str().to_owned(),
    ]);
    // FSKit opens the device with the caller's credentials, so a raw disk
    // needs root here (disk images attached by the user do not).
    let status = if disk.contains("disk") && !is_user_owned_device(&bsd_name(disk)) {
        privileged("/sbin/mount", &args)?
    } else {
        Command::new("/sbin/mount").args(&args).status()?
    };
    if !status.success() {
        return Err(format!("mount failed. {ENABLE_HINT}").into());
    }
    println!("mounted at {}", mountpoint.display());
    Ok(())
}

fn is_user_owned_device(bsd: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let uid = fs::metadata(".").map(|m| m.uid()).unwrap_or(u32::MAX);
    fs::metadata(format!("/dev/{bsd}"))
        .map(|m| m.uid() == uid && uid != 0)
        .unwrap_or(false)
}

fn unmount(target: &str, force: bool) -> CliResult<()> {
    let path = Path::new(target);
    let bsd = if path.is_absolute() && !target.starts_with("/dev/") {
        device_at(path).ok_or_else(|| format!("nothing is mounted at {target}"))?
    } else {
        bsd_name(target)
    };
    da::unmount(&bsd, force).map_err(|e| format!("unmount of {bsd} failed: {e}"))?;
    println!("unmounted {bsd}");
    Ok(())
}

/// Split `diskNsM` into (`diskN`, M).
fn split_partition(bsd: &str) -> Option<(String, u32)> {
    let rest = bsd.strip_prefix("disk")?;
    let (disk, part) = rest.split_once('s')?;
    if disk.is_empty() || !disk.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((format!("disk{disk}"), part.parse().ok()?))
}

/// `Partition Offset` from `diskutil info`, in bytes.
fn partition_offset(bsd: &str) -> CliResult<u64> {
    let out = Command::new("diskutil").args(["info", bsd]).output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.trim().strip_prefix("Partition Offset:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("diskutil does not report a partition offset for {bsd}").into())
}

fn set_type(target: &str, type_name: &str, index: Option<u32>, yes: bool) -> CliResult<()> {
    let guid = match type_name {
        "linux" => gpt::LINUX_FILESYSTEM.to_string(),
        other => {
            gpt::guid_to_bytes(other)?;
            other.to_uppercase()
        }
    };
    let image = Path::new(target).is_file();
    let (disk_path, index, expect_offset) = if image {
        let index = index.ok_or("an image file needs --index <partition number>")?;
        (PathBuf::from(target), index, None)
    } else {
        let bsd = bsd_name(target);
        let (disk, part) = split_partition(&bsd).ok_or("give a partition such as disk8s2")?;
        if index.is_some_and(|i| i != part) {
            return Err("--index does not match the partition name".into());
        }
        let mounted = Command::new("/sbin/mount").output()?;
        let prefix = format!("/dev/{disk}s");
        if String::from_utf8_lossy(&mounted.stdout)
            .lines()
            .any(|l| l.starts_with(&prefix))
        {
            return Err(format!(
                "a partition of {disk} is mounted; unmount it first (ext4 unmount {bsd})"
            )
            .into());
        }
        (
            PathBuf::from(format!("/dev/r{disk}")),
            part,
            Some(partition_offset(&bsd)?),
        )
    };
    let device = FileDevice::open(&disk_path, true).map_err(|e| {
        if raw_access_denied(&e) {
            format!("{}: permission denied; run with sudo", disk_path.display())
        } else {
            e.to_string()
        }
    })?;
    // Raw disks accept only whole-sector transfers; the GPT code reads and
    // writes small fields, so go through the read-modify-write adapter.
    let device = ext4_core::device::AlignedDevice::new(device);
    let (sector, entries) = gpt::list(&device)?;
    let entry = entries
        .iter()
        .find(|e| e.index == index)
        .ok_or_else(|| format!("partition {index} does not exist"))?;
    let expect_lba = match expect_offset {
        Some(off) if off % sector != 0 => {
            return Err("partition offset is not sector aligned".into())
        }
        Some(off) => Some(off / sector),
        None => None,
    };
    if entry.type_guid == guid {
        println!("partition {index} already has type {guid}");
        return Ok(());
    }
    if !yes {
        eprint!(
            "Change partition {index} of {} ({} sectors from {}) from type {} to {guid}? Data is not touched [y/N] ",
            disk_path.display(),
            entry.last_lba - entry.first_lba + 1,
            entry.first_lba,
            entry.type_guid
        );
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            return Err("cancelled".into());
        }
    }
    let before = gpt::set_type(&device, index, &guid, expect_lba)?;
    println!("partition {index}: type {} -> {guid}", before.type_guid);
    if !image {
        println!("Eject and reconnect the disk so macOS reads the new partition table.");
    }
    Ok(())
}

fn ls(fs: &Filesystem, path: &str, long: bool) -> CliResult<()> {
    let attr = resolve(fs, path)?;
    if attr.file_type != FileType::Directory {
        return print_entry(fs, path.as_bytes(), &attr, long);
    }
    for entry in fs.list_dir(attr.ino)? {
        if entry.name == b"." || entry.name == b".." {
            continue;
        }
        let child = fs.getattr(entry.inode)?;
        print_entry(fs, &entry.name, &child, long)?;
    }
    Ok(())
}

fn print_entry(_fs: &Filesystem, name: &[u8], attr: &Attr, long: bool) -> CliResult<()> {
    if long {
        println!(
            "{:o} {:>3} {:>5}:{:<5} {:>10} {}",
            attr.mode,
            attr.nlink,
            attr.uid,
            attr.gid,
            attr.size,
            String::from_utf8_lossy(name)
        );
    } else {
        println!("{}", String::from_utf8_lossy(name));
    }
    Ok(())
}

fn cat(fs: &Filesystem, path: &str) -> CliResult<()> {
    let attr = resolve(fs, path)?;
    if attr.file_type != FileType::Regular {
        return Err("cat requires a regular file".into());
    }
    let mut out = io::stdout().lock();
    read_file(fs, attr.ino, attr.size, |data| {
        out.write_all(data).map_err(Into::into)
    })
}

fn get(fs: &Filesystem, path: &str, destination: &Path) -> CliResult<()> {
    let attr = resolve(fs, path)?;
    export_item(fs, attr.ino, destination)
}

fn export_item(fs: &Filesystem, ino: u32, destination: &Path) -> CliResult<()> {
    let attr = fs.getattr(ino)?;
    match attr.file_type {
        FileType::Directory => {
            fs::create_dir_all(destination)?;
            for entry in fs.list_dir(ino)? {
                if entry.name == b"." || entry.name == b".." {
                    continue;
                }
                export_item(fs, entry.inode, &destination.join(bytes_to_os(&entry.name)))?;
            }
        }
        FileType::Regular => {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = fs::File::create(destination)?;
            read_file(fs, ino, attr.size, |data| {
                output.write_all(data).map_err(Into::into)
            })?;
        }
        FileType::Symlink => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                symlink(bytes_to_os(&fs.readlink(ino)?), destination)?;
            }
            #[cfg(not(unix))]
            return Err("symlink extraction requires Unix".into());
        }
        _ => return Err("get supports regular files, directories, and symlinks".into()),
    }
    Ok(())
}

fn put(fs: &Filesystem, host_source: &Path, destination: &str) -> CliResult<()> {
    let (parent, name) = resolve_parent(fs, destination)?;
    import_item(fs, host_source, parent, &name)
}

fn import_item(fs: &Filesystem, source: &Path, parent: u32, name: &[u8]) -> CliResult<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_dir() {
        let dir = fs.mkdir(parent, name, host_permissions(&metadata, 0o755), 0, 0)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            import_item(fs, &entry.path(), dir.ino, &os_to_bytes(entry.file_name()))?;
        }
        return Ok(());
    }
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source)?;
        fs.symlink(parent, name, &os_to_bytes(target.into_os_string()), 0, 0)?;
        return Ok(());
    }
    if !metadata.file_type().is_file() {
        return Err("put supports regular files, directories, and symlinks".into());
    }
    let file = fs.create(
        parent,
        name,
        S_IFREG | host_permissions(&metadata, 0o644),
        0,
        0,
        (0, 0),
    )?;
    let mut input = fs::File::open(source)?;
    let mut offset = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let written = fs.write(file.ino, offset, &buffer[..count])?;
        if written != count {
            return Err("short write to ext filesystem".into());
        }
        offset += written as u64;
    }
    Ok(())
}

fn mkdir(fs: &Filesystem, path: &str) -> CliResult<()> {
    let (parent, name) = resolve_parent(fs, path)?;
    fs.mkdir(parent, &name, 0o755, 0, 0)?;
    Ok(())
}

fn remove(fs: &Filesystem, path: &str, recursive: bool) -> CliResult<()> {
    let (parent, name) = resolve_parent(fs, path)?;
    let target = fs.lookup(parent, &name)?;
    if target.file_type == FileType::Directory {
        let entries = fs.list_dir(target.ino)?;
        let children: Vec<_> = entries
            .into_iter()
            .filter(|entry| entry.name != b"." && entry.name != b"..")
            .collect();
        if !children.is_empty() && !recursive {
            return Err("directory is not empty; pass -r".into());
        }
        for child in children {
            remove_by_inode(fs, target.ino, &child.name, true)?;
        }
    }
    let ino = fs.remove(parent, &name)?;
    fs.reclaim(ino)?;
    Ok(())
}

fn remove_by_inode(fs: &Filesystem, parent: u32, name: &[u8], recursive: bool) -> CliResult<()> {
    let target = fs.lookup(parent, name)?;
    if target.file_type == FileType::Directory && recursive {
        for child in fs.list_dir(target.ino)? {
            if child.name != b"." && child.name != b".." {
                remove_by_inode(fs, target.ino, &child.name, true)?;
            }
        }
    }
    let ino = fs.remove(parent, name)?;
    fs.reclaim(ino)?;
    Ok(())
}

fn rename(fs: &Filesystem, source: &str, destination: &str) -> CliResult<()> {
    let (source_parent, source_name) = resolve_parent(fs, source)?;
    let (destination_parent, destination_name) = resolve_parent(fs, destination)?;
    fs.rename(
        source_parent,
        &source_name,
        destination_parent,
        &destination_name,
        false,
        false,
    )?;
    Ok(())
}

fn symlink(fs: &Filesystem, target: &str, path: &str) -> CliResult<()> {
    let (parent, name) = resolve_parent(fs, path)?;
    fs.symlink(parent, &name, target.as_bytes(), 0, 0)?;
    Ok(())
}

fn stat(fs: &Filesystem, path: &str) -> CliResult<()> {
    let attr = resolve(fs, path)?;
    println!("inode: {}", attr.ino);
    println!("type: {:?}", attr.file_type);
    println!("mode: {:o}", attr.mode);
    println!("uid: {}", attr.uid);
    println!("gid: {}", attr.gid);
    println!("links: {}", attr.nlink);
    println!("size: {}", attr.size);
    println!("allocated: {}", attr.alloc_bytes);
    println!("parent inode: {}", attr.parent);
    println!("mtime: {}.{}", attr.mtime.secs, attr.mtime.nsecs);
    Ok(())
}

fn resolve(fs: &Filesystem, path: &str) -> CliResult<Attr> {
    let mut attr = fs.getattr(ROOT_INO)?;
    for part in path_components(path)? {
        attr = fs.lookup(attr.ino, part)?;
    }
    Ok(attr)
}

fn resolve_parent(fs: &Filesystem, path: &str) -> CliResult<(u32, Vec<u8>)> {
    let mut parts = path_components(path)?;
    let name = parts
        .pop()
        .ok_or("path must name an item below the root")?
        .to_vec();
    let mut parent = ROOT_INO;
    for part in parts {
        parent = fs.lookup(parent, part)?.ino;
    }
    Ok((parent, name))
}

fn path_components(path: &str) -> CliResult<Vec<&[u8]>> {
    let mut components = Vec::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err("parent traversal is not allowed".into());
        }
        components.push(component.as_bytes());
    }
    Ok(components)
}

fn read_file(
    fs: &Filesystem,
    ino: u32,
    size: u64,
    mut write: impl FnMut(&[u8]) -> CliResult<()>,
) -> CliResult<()> {
    let mut offset = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    while offset < size {
        let wanted = (size - offset).min(buffer.len() as u64) as usize;
        let count = fs.read(ino, offset, &mut buffer[..wanted])?;
        if count == 0 {
            return Err("unexpected end of file".into());
        }
        write(&buffer[..count])?;
        offset += count as u64;
    }
    Ok(())
}

fn host_permissions(metadata: &fs::Metadata, _fallback: u16) -> u16 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (metadata.permissions().mode() as u16) & 0o7777
    }
    #[cfg(not(unix))]
    {
        _fallback
    }
}

fn plist_values(text: &str, key: &str) -> Vec<String> {
    let marker = format!("<key>{key}</key>");
    text.match_indices(&marker)
        .filter_map(|(index, _)| {
            text[index + marker.len()..]
                .split_once("<string>")
                .and_then(|(_, rest)| {
                    rest.split_once("</string>")
                        .map(|(value, _)| value.to_owned())
                })
        })
        .collect()
}

fn is_mounted(dev: &str) -> CliResult<bool> {
    let Some(name) = disk_name(dev) else {
        return Ok(false);
    };
    let output = Command::new("diskutil")
        .args(["info", "-plist", &name])
        .output()?;
    if !output.status.success() {
        return Err(
            format!("diskutil info {name} failed; cannot check whether it is mounted").into(),
        );
    }
    Ok(
        plist_values(&String::from_utf8_lossy(&output.stdout), "MountPoint")
            .iter()
            .any(|value| !value.is_empty()),
    )
}

fn is_whole_disk_with_partitions(dev: &str) -> CliResult<bool> {
    let Some(name) = whole_disk_name(dev) else {
        return Ok(false);
    };
    let output = Command::new("diskutil")
        .args(["list", "-plist", &name])
        .output()?;
    if !output.status.success() {
        return Err(format!("diskutil list {name} failed; cannot check its partitions").into());
    }
    Ok(
        plist_values(&String::from_utf8_lossy(&output.stdout), "DeviceIdentifier")
            .iter()
            .any(|entry| entry.starts_with(&format!("{name}s"))),
    )
}

/// `diskN` if `dev` names a whole disk, not a partition or a file.
fn whole_disk_name(dev: &str) -> Option<String> {
    disk_name(dev).filter(|name| split_partition(name).is_none())
}

/// `diskN` or `diskNsM` for a BSD device argument (`disk4`, `/dev/rdisk4s2`, ...).
fn disk_name(dev: &str) -> Option<String> {
    let name = dev.strip_prefix("/dev/").unwrap_or(dev);
    let name = name.strip_prefix('r').unwrap_or(name);
    let rest = name.strip_prefix("disk")?;
    rest.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_digit())
        .then(|| name.to_owned())
}

fn privileged(program: &str, args: &[OsString]) -> io::Result<std::process::ExitStatus> {
    let mut command = if std::env::var_os("USER").as_deref() == Some(std::ffi::OsStr::new("root")) {
        Command::new(program)
    } else {
        let mut command = Command::new("sudo");
        command.arg(program);
        command
    };
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
}

fn raw_access_denied(error: impl std::fmt::Display) -> bool {
    let text = error.to_string();
    text.contains("Permission denied") || text.contains("os error 13")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn bytes_to_os(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8_lossy(bytes).into_owned().into()
    }
}

fn os_to_bytes(value: OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        value.into_vec()
    }
    #[cfg(not(unix))]
    {
        value.to_string_lossy().into_owned().into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::whole_disk_name;

    #[test]
    fn mkfs_guard_inspects_whole_disks_only() {
        // The "s" in "disk" must not make diskN look like a partition.
        for (dev, whole) in [
            ("disk4", "disk4"),
            ("/dev/disk4", "disk4"),
            ("/dev/rdisk42", "disk42"),
        ] {
            assert_eq!(whole_disk_name(dev).as_deref(), Some(whole), "{dev}");
        }
        for dev in ["disk4s2", "/dev/rdisk4s2", "disk.img", "image.img"] {
            assert_eq!(whole_disk_name(dev), None, "{dev}");
        }
    }
}
