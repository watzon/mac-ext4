# Ext4 for macOS

This directory contains the FSKit host app and file system extension. The Swift layer only adapts FSKit to the Rust C ABI in `crates/ext4-ffi/include/ext4fs.h`; the Rust engine owns ext2, ext3, and ext4 parsing and mutation.

## Prerequisites

- macOS 15.4 or later and Xcode 27.1.
- XcodeGen, normally installed at `/opt/homebrew/bin/xcodegen`.
- A paid Apple Developer team with the **FSKit File System Module** capability enabled for the App ID. Set that team's ID in `macos/Config/Local.xcconfig`.
- Rust and the targets needed by the architectures you build:

  ```sh
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  ```

Copy the tracked template and replace the values:

```sh
cp macos/Config/Local.xcconfig.example macos/Config/Local.xcconfig
```

`DEVELOPMENT_TEAM` must be the paid-team ID that owns the FSKit capability. `EXT4_BUNDLE_PREFIX` defaults to `dev.watzon.macext4`; use a unique reverse-DNS prefix for a distributed build.

## Build and install

Generate the project and build with automatic provisioning. Xcode must be signed in (Xcode > Settings > Accounts) to the team in `Local.xcconfig`, so that it can create the FSKit provisioning profile and register this Mac:

```sh
xcodegen generate --spec macos/project.yml
xcodebuild -project macos/Ext4.xcodeproj -scheme Ext4 -configuration Release \
  -allowProvisioningUpdates -allowProvisioningDeviceRegistration build
```

Or run the idempotent installer:

```sh
scripts/install.sh
```

The installer builds the Rust FFI static library for the Xcode `ARCHS`, embeds `Ext4FS.appex` in `Ext4.app`, copies the host app to `/Applications`, launches it once to register the extension, and restarts the per-user `fskit_agent` (without that restart, macOS keeps a stale module list after an upgrade and reports "No extension with fsShortName found"). It asks for administrator authorization only when `/Applications` is not writable.

Enable the extension once:

1. Open **System Settings > General > Login Items & Extensions**.
2. Switch the extension list to **By Category**.
3. Click the info button next to **File System Extensions** and turn on **Ext4**.

The per-app view (**Ext4 Extensions > FSKit Modules**) shows a toggle that ignores clicks on current macOS releases. Use the category view. Check registration with:

```sh
pluginkit -m -p com.apple.fskit.fsmodule -v
```

The installer also copies `macos/Filesystems/ext4.fs` to `/Library/Filesystems/ext4.fs` (it asks for your password the first time). macOS takes the format name that Finder shows ("Linux ext4") from `Filesystems/<type>.fs` bundles, not from FSKit modules; without it Finder shows "Unknown (ext4)". The bundle holds names only: no media types and no programs, so probing, mounting, checking, and formatting stay with the FSKit module. Finder reads these bundles when it starts, so relaunch Finder (`killall Finder`) after the first install.

A `+` in front of `dev.watzon.macext4.fsmodule` means the extension is elected. FSKit keeps its own enabled list in `~/Library/Group Containers/group.com.apple.fskit.settings/enabledModules.plist`.

Remove the installed app and unregister its extension with:

```sh
scripts/uninstall.sh
```

## Building the installer package

`scripts/package.sh` builds `dist/Ext4-<version>.pkg`, which installs:

- `/Applications/Ext4.app`, exported with Developer ID signing (its embedded provisioning profile covers all Macs);
- `/Library/Filesystems/ext4.fs`, the Finder format names;
- `/usr/local/bin/ext4`, the command-line tool (universal, Developer ID signed).

The installer unmounts ext volumes before it replaces the module and mounts them again afterwards, restarts `fskit_agent` and Finder, and opens Ext4.app, which shows the setup checklist and the button to the System Settings pane. Its welcome and finish pages (`macos/pkg/resources`) explain the one manual step.

The version comes from `Cargo.toml`. `EXT4_SIGNING` selects how the script signs: `automatic` (the default; the provisioning profile that Xcode, or an App Store Connect API key, makes), `manual` (the profile file in `EXT4_FSMODULE_PROFILE`), or `none` (no signatures; CI uses this to check the build). The script signs the pkg when it finds a **Developer ID Installer** certificate for the team (or set `INSTALLER_IDENTITY`), and notarizes and staples it when `NOTARY_PROFILE` or `APPLE_API_KEY_*` is set. Without these, the pkg works only on the build Mac. [docs/releasing.md](../docs/releasing.md) describes the credentials and the Release workflow.

The app icon is an Icon Composer document, `macos/Ext4/AppIcon.icon`. Its flat layer artwork comes from `tools/icon/layers.swift`; Liquid Glass, colors, and the dark-appearance label are set in `icon.json`. Preview it with `ictool` (inside Icon Composer.app) or open it in Icon Composer.

## Mounting and unmounting

Plug in a disk with an ext2, ext3, or ext4 partition: Disk Arbitration runs a quick check (`startCheck -q`) and mounts it under `/Volumes/<label>`. After an unclean shutdown, the quick check fails and Disk Arbitration runs a repair, which replays the journal and processes orphans (it scans the whole volume only when the file system has its error flag set).

From a terminal, use the CLI, which talks to Disk Arbitration and needs no `sudo`:

```sh
ext4 mount disk8s2          # checks and mounts at /Volumes/<label>
ext4 mount -r disk8s2       # read-only
ext4 unmount /Volumes/SHARED
```

Right after an FSKit volume unmounts, Disk Arbitration probes the disk again before the old module instance has shut down; that probe fails and the disk shows as unsupported until the next request. `ext4 mount` retries once for this case.

Plain `umount` and `diskutil unmount` fail for these volumes: Disk Arbitration mounts them as root, and StorageKit refuses partitions whose type it does not know (such as "Microsoft Basic Data"). Ejecting from Finder works.

`mount -F -t ext4 diskNsM /path` also works. FSKit opens the device with the caller's credentials, so a physical disk needs `sudo`; disk images attached by the user do not.

Use the BSD identifier of the partition from `diskutil list`, not the whole disk.

## Formatting

Disk Utility can erase drives and partitions as **Linux ext4**. (ext2 and ext3 are left out of its menu on purpose: Disk Utility gives every format with a distinct internal type its own menu section, and only Apple's ExFAT and MS-DOS are grouped by a hardcoded exception. Use `ext4 mkfs -t ext3` or `newfs_fskit -t ext3` for those.) Its Erase menu lists formats from `/Library/Filesystems/*.fs` bundles that declare `FSFormatExecutable`; FSKit modules are not consulted. `ext4.fs` declares `Contents/Resources/newfs_ext4`, which is the `ext4` CLI behaving as a `newfs` formatter (`-v NAME`, `-t ext2|ext3|ext4`, device last, no prompts). Erasing a whole drive with the GUID scheme creates the partition with the Linux file system type. `FSFormatContentMask` must be MediaKit's type name `Linux`: with the GUID (or "Linux Filesystem") there, Disk Utility also carves an Apple_Boot "Boot OS X" partition out of the target. The same works from Terminal without `sudo`, because storagekitd runs the formatter:

```sh
diskutil listFilesystems                       # shows ext4 ("Linux ext4")
diskutil eraseDisk ext4 DATA GPT disk8         # whole drive, Linux partition type
diskutil eraseVolume ext4 DATA disk8s2         # one partition
```

`newfs_fskit` dispatches to the FSKit module's `startFormat` instead. It supports `-L` label, `-b` block size, `-I` inode size, `-i` bytes per inode, `-m` reserved-block percentage, and `-t ext2|ext3|ext4`, and needs `sudo` for physical disks because FSKit opens the device with your credentials:

```sh
diskutil unmount disk8s2
sudo newfs_fskit --progress -t ext4 -L data disk8s2
```

Formatting irreversibly replaces the selected volume. Verify the BSD identifier before running it.

A partition formatted in place keeps its GPT type (for example "Microsoft Basic Data"). The module claims that type as well as the Linux file system type (`0FC63DAF-8483-4772-8E79-3D69D8477DE4`) and probes them before the FAT and exFAT modules. To mark the partition as Linux, unmount it and run:

```sh
ext4 unmount disk8s2
sudo ext4 set-type disk8s2      # Linux filesystem; or -t <GUID>
```

Then eject and reconnect the disk. `set-type` changes only the type field of that one entry, in both the primary and the backup table, and checks that the entry starts where `diskutil` says the partition starts.

## Command-line tool

Build the offline tool with:

```sh
cargo build -p ext4-cli
```

`target/debug/ext4` can inspect and manipulate image files without the FSKit extension:

```sh
ext4 list
ext4 info target/img/test.img
ext4 mkfs -f -t ext4 -L test target/img/test.img
ext4 put target/img/test.img ./source-tree /tree
ext4 ls -l target/img/test.img /tree
ext4 get target/img/test.img /tree ./recovered-tree
ext4 check -n target/img/test.img
```

`put`, `mkdir`, `rm`, `mv`, and `ln -s` mount the image read-write internally and unmount it cleanly. Path traversal is rooted at the filesystem root and does not follow symbolic links. For raw disks, use `diskN`, `diskNsM`, `/dev/diskNsM`, or `/dev/rdiskNsM`; the tool selects the raw device form. `mkfs` refuses mounted devices and partitioned whole disks unless `-f`, and requests confirmation unless `-f` or `--yes` is passed.

LaunchServices registers every `Ext4.app` that a build produces. If `fskit_agent` picks one of these copies, RunningBoard refuses to start the module ("Client not authorized" in the `fskit_agent` log) and mounts fail with "unsupported". `scripts/install.sh` and `scripts/package.sh` unregister their build copies. To find stray registrations:

```sh
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -dump | grep -E '^path:.*Ext4'
```

## Debugging

Stream module logs with:

```sh
log stream --style compact --predicate 'subsystem == "dev.watzon.macext4.fsmodule"'
```

The extension uses FSKit direct block-device I/O exclusively. It deliberately does not mix those reads and writes with FSKit metadata cache APIs.
