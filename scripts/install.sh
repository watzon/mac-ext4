#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
project_dir="${repo_root}/macos"
app_source="${project_dir}/build/Build/Products/Release/Ext4.app"
app_destination="/Applications/Ext4.app"

if [[ ! -x /opt/homebrew/bin/xcodegen ]] && ! command -v xcodegen >/dev/null 2>&1; then
    echo "error: xcodegen is required. Install it before running this script." >&2
    exit 1
fi

xcodegen_bin="$(command -v xcodegen || true)"
if [[ -z "${xcodegen_bin}" ]]; then
    xcodegen_bin="/opt/homebrew/bin/xcodegen"
fi

"${xcodegen_bin}" generate --spec "${project_dir}/project.yml" --project "${project_dir}"
xcodebuild -project "${project_dir}/Ext4.xcodeproj" -scheme Ext4 -configuration Release -derivedDataPath "${project_dir}/build" -allowProvisioningUpdates -allowProvisioningDeviceRegistration build

if [[ ! -d "${app_source}" ]]; then
    echo "error: Release build did not produce ${app_source}" >&2
    exit 1
fi

# Unmount ext volumes first: macOS stops the running module process when its
# bundle is replaced, which would leave a dead mount behind.
cargo build --manifest-path "${repo_root}/Cargo.toml" --release -p ext4-cli --quiet
cli="${repo_root}/target/release/ext4"
/sbin/mount | while IFS= read -r line; do
    case "$line" in
        *"(ext2,"*fskit*|*"(ext3,"*fskit*|*"(ext4,"*fskit*) ;;
        *) continue ;;
    esac
    mnt="${line#* on }"
    mnt="${mnt% (*}"
    echo "Unmounting ${mnt} before replacing the module"
    "${cli}" unmount "${mnt}" || { echo "error: could not unmount ${mnt}; close files on it and retry" >&2; exit 1; }
done

copy_app() {
    /usr/bin/ditto "${app_source}" "${app_destination}"
}

if [[ -w /Applications && ( ! -e "${app_destination}" || -w "${app_destination}" ) ]]; then
    copy_app
else
    echo "Installing to /Applications requires administrator authorization."
    sudo /usr/bin/ditto "${app_source}" "${app_destination}"
fi

# LaunchServices registered the build copy too. fskit_agent may start the
# module from it, which RunningBoard refuses ("Client not authorized").
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -u "${app_source}" 2>/dev/null || true

# Finder takes format names from Filesystems/<type>.fs bundles, not from
# FSKit modules. Install a metadata-only bundle so it shows "Linux ext4".
# Its Contents/Resources/newfs_ext4 is the CLI, which Disk Utility runs to erase
# a disk as ext4.
fs_source="$(mktemp -d)/ext4.fs"
/usr/bin/ditto "${project_dir}/Filesystems/ext4.fs" "${fs_source}"
mkdir -p "${fs_source}/Contents/Resources"
install -m 0755 "${cli}" "${fs_source}/Contents/Resources/newfs_ext4"
fs_destination="/Library/Filesystems/ext4.fs"
if ! /usr/bin/cmp -s "${fs_source}/Contents/Info.plist" "${fs_destination}/Contents/Info.plist" 2>/dev/null \
    || ! /usr/bin/cmp -s "${fs_source}/Contents/Resources/newfs_ext4" "${fs_destination}/Contents/Resources/newfs_ext4" 2>/dev/null; then
    echo "Installing ${fs_destination} (the format name Finder shows) requires administrator authorization."
    sudo /bin/mkdir -p /Library/Filesystems
    sudo /bin/rm -rf "${fs_destination}"
    sudo /usr/bin/ditto "${fs_source}" "${fs_destination}"
    sudo /usr/sbin/chown -R root:wheel "${fs_destination}"
    # storagekitd caches the list of formattable file systems.
    sudo /usr/bin/killall storagekitd 2>/dev/null || true
fi

# fskit_agent caches the module list by bundle location. After the bundle is
# replaced it reports "No extension with fsShortName found" until restarted.
# It is a per-user agent that launchd starts again on demand.
pkill -9 -x fskit_agent 2>/dev/null || true
# Give launchd a moment to bring the agent back before the first mount.
sleep 3
open "${app_destination}"
echo "Ext4 is installed. Enable it once in System Settings > General > Login Items & Extensions:"
echo "switch the list to 'By Category', click the (i) next to File System Extensions, and turn on Ext4."
echo "(The per-app 'FSKit Modules' toggle in the 'By App' view does not work on current macOS.)"
echo "Verify registration with: pluginkit -m -p com.apple.fskit.fsmodule -v"
