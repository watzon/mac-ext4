#!/usr/bin/env bash
set -euo pipefail

app_destination="/Applications/Ext4.app"
extension_path="${app_destination}/Contents/Extensions/Ext4FS.appex"
fs_bundle="/Library/Filesystems/ext4.fs"

if [[ -d "${extension_path}" ]]; then
    pluginkit -r "${extension_path}" || echo "warning: pluginkit could not unregister the extension; it may already be unregistered." >&2
fi

if [[ -d "${fs_bundle}" ]]; then
    echo "Removing ${fs_bundle} requires administrator authorization."
    sudo /bin/rm -rf "${fs_bundle}"
fi

if [[ ! -e "${app_destination}" ]]; then
    echo "Ext4 is not installed in /Applications. Nothing to remove."
    exit 0
fi

remove_app() {
    /bin/rm -rf "${app_destination}"
}

if [[ -w /Applications && -w "${app_destination}" ]]; then
    remove_app
else
    echo "Removing /Applications/Ext4.app requires administrator authorization."
    sudo /bin/rm -rf "${app_destination}"
fi

echo "Ext4 was removed. If it remains visible in System Settings, disable it there or restart System Settings."
