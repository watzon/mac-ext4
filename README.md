# mac-ext4

mac-ext4 reads and writes Linux ext2, ext3, and ext4 file systems on macOS,
and formats disks as ext4. It is a file system module for FSKit, so it needs
no kernel extension, no macFUSE, and no change to the security settings of
your Mac.

Plug in a disk with an ext4 partition and it mounts in Finder like any other
disk. Disk Utility can erase a disk as "Linux ext4".

The file system code is a new implementation in Rust, written from the
public description of the on-disk format. It does not contain code from
Linux or e2fsprogs.

> [!NOTE]
> **This is new software. Keep a backup of data that you put on an ext4 disk
> with it.**

## Requirements

- macOS 15.4 or later
- A Mac with Apple silicon or an Intel processor

## Installation

1. Download `Ext4-<version>.pkg` from the releases page of this repository
   and open it.
2. When the installer is done, it opens the Ext4 app. Do the one step that
   the app shows: open **System Settings > General > Login Items &
   Extensions**, change the list to **By Category**, click the info button
   next to **File System Extensions**, and turn on **Ext4**.

Use the **By Category** list. On current macOS releases, the switch in the
**By App** list (Ext4 > FSKit Modules) does not respond to clicks.

The installer puts these items on your Mac:

- `/Applications/Ext4.app`, which contains the file system module
- `/Library/Filesystems/ext4.fs`, which gives Finder and Disk Utility the
  format names and the formatter
- `/usr/local/bin/ext4`, the command-line tool

To remove mac-ext4, delete the three items, then restart the Mac.

## Usage

### Mounting

Connect the disk. macOS checks the file system and mounts it at
`/Volumes/<label>`. After an unclean shutdown, the check replays the
journal first, as `fsck -p` does on Linux.

Eject the disk in Finder before you disconnect it.

From Terminal (no `sudo` needed):

```sh
ext4 list                       # ext partitions on connected disks
ext4 mount disk4s2              # mount at /Volumes/<label>
ext4 mount -r disk4s2           # mount read-only
ext4 unmount /Volumes/DATA
```

Plain `umount` and `diskutil unmount` do not work for these volumes. Use
`ext4 unmount` or eject in Finder.

### Formatting

In Disk Utility, select the disk or the partition, click **Erase**, and
select **Linux ext4** as the format. Or, from Terminal:

```sh
diskutil eraseDisk ext4 DATA GPT disk4      # the full disk
diskutil eraseVolume ext4 DATA disk4s2      # one partition
```

Both commands erase all data on the target. Use `diskutil list` to make sure
that you have the correct disk.

For ext2 or ext3, or to set the block size or the inode size, use the
command-line tool:

```sh
sudo ext4 mkfs -t ext3 -L DATA disk4s2
ext4 mkfs --help
```

If you format a partition that had a different type (for example "Microsoft
Basic Data"), the partition keeps that type. mac-ext4 mounts it, but Linux
and other tools can show the wrong type. To change it to "Linux
filesystem", unmount the partition, then:

```sh
sudo ext4 set-type disk4s2
```

Disconnect the disk and connect it again after this command.

### Disk images and files without mounting

The `ext4` tool can also read and change a file system in a disk image or on
a disk that is not mounted:

```sh
mkfile -n 1g test.img                       # an empty 1 GB image file
ext4 mkfs --yes -t ext4 -L test test.img
ext4 ls -l test.img /
ext4 put test.img ./photos /photos
ext4 get test.img /photos ./photos-copy
ext4 check test.img
```

`ext4 help` lists all commands.

## Supported features

Read and write:

- ext2, ext3, and ext4
- extents, 64-bit block numbers, flexible block groups, huge files
- metadata checksums (`metadata_csum`, `metadata_csum_seed`)
- hashed directories (`dir_index`), inline data, extended attributes
- the jbd2 journal, with replay after a crash
- orphan files and the orphan list

Read-only: file systems with `bigalloc`, quotas, case-folding, encryption,
large extended attribute values (`ea_inode`), multi-mount protection,
`verity`, or an external journal. These mount read-only.

Not supported: fast-commit journals that need replay.

## Limitations

- Speed. File data goes through FSKit read and write calls. FSKit can
  also give the kernel direct access to the blocks, which is faster.
  mac-ext4 does not use that yet.
- Write cache. FSKit has no call that flushes the write cache of the
  disk. If the power fails, a disk with a volatile write cache can write
  blocks in a different order than mac-ext4 sent them. The journal
  prevents most, but not all, damage of this type.
- Disk Utility shows only "Linux ext4". Disk Utility puts each format in
  its own section of the menu, so ext2 and ext3 are available only from
  `ext4 mkfs`.

## Building from source

You need Xcode 27, Rust, and [XcodeGen](https://github.com/yonaskolb/XcodeGen)
(`brew install xcodegen`). You also need a paid Apple Developer account:
macOS loads FSKit modules only with a provisioning profile that allows the
FSKit Module capability.

```sh
cp macos/Config/Local.xcconfig.example macos/Config/Local.xcconfig
# Set DEVELOPMENT_TEAM in Local.xcconfig to your team ID.
scripts/install.sh
```

Xcode must be signed in to that team (Xcode > Settings > Accounts).
`scripts/install.sh` builds the app and installs it on this Mac.
`scripts/package.sh` builds the installer package. See
[macos/README.md](macos/README.md) for the details, and
[docs/releasing.md](docs/releasing.md) for the release process.

The source is in these directories:

- `crates/ext4-core`: the file system code (reading, writing, the
  journal, `mkfs`, and the checker). It has no macOS dependency.
- `crates/ext4-ffi`: a C interface to `ext4-core` for the FSKit module.
- `crates/ext4-cli`: the `ext4` command.
- `macos`: the app and the FSKit module, in Swift.

## Testing

```sh
brew install e2fsprogs gptfdisk
cargo test --workspace
```

The tests use the Linux tools (`mke2fs`, `e2fsck`, `debugfs`, `sgdisk`) as a
reference. Each test makes a file system with one implementation and checks
it with the other. `tools/e2fs/run` runs the tools from Homebrew, or from a
Debian image when Docker is running. If neither is available, these tests
are skipped.

## Reporting bugs

Open an issue. Include the macOS version, the output of `ext4 info` for the
disk, and the module log:

```sh
log show --last 10m --predicate 'subsystem == "dev.watzon.macext4.fsmodule"'
```

If the problem is with a specific file system, and you can share it, attach
an image of it (for example, made with `dd`). A small image that shows the
problem is best.

## License

Apache License 2.0. See [LICENSE](LICENSE).
