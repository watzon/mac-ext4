#!/usr/bin/env bash
# Build the installer: dist/Ext4-<version>.pkg and dist/Ext4-<version>.pkg.sha256
#
# The package installs /Applications/Ext4.app (with the FSKit module),
# /Library/Filesystems/ext4.fs (Finder names and the Disk Utility formatter),
# and /usr/local/bin/ext4. The version comes from Cargo.toml.
#
# Signing, chosen with EXT4_SIGNING:
#   automatic  (default) Developer ID, with the provisioning profile that Xcode
#              makes and keeps current. Xcode must be signed in to the team.
#              Use this on your own Mac only: on a CI runner, Xcode makes a
#              new signing certificate each time and the runner deletes its
#              private key.
#   manual     Developer ID, with the provisioning profile file named by
#              EXT4_FSMODULE_PROFILE. The profile must be a manually managed
#              Developer ID profile from the developer portal. The release
#              workflow uses this; Xcode changes nothing in the account.
#   none       No signatures at all, for CI checks of the packaging. The
#              package cannot run on another Mac.
#
# Other settings:
#   EXT4_TEAM_ID          team ID (default: DEVELOPMENT_TEAM in macos/Config/Local.xcconfig)
#   EXT4_BUILD_NUMBER     CFBundleVersion (default: number of commits, or 1)
#   INSTALLER_IDENTITY    pkg signing identity (default: the "Developer ID Installer" of the team)
#   NOTARY_PROFILE        notarytool keychain profile, or
#   APPLE_API_KEY_PATH, APPLE_API_KEY_ID, APPLE_API_ISSUER_ID   an App Store Connect API key
#   EXT4_REQUIRE_SIGNED=1 fail unless the package is signed and notarized
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
macos="${repo_root}/macos"
work="${macos}/build/dist"
dist="${repo_root}/dist"
signing="${EXT4_SIGNING:-automatic}"
version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "${repo_root}/Cargo.toml" | head -1)"
build_number="${EXT4_BUILD_NUMBER:-$(git -C "${repo_root}" rev-list --count HEAD 2>/dev/null || echo 1)}"
# CI has no Local.xcconfig; the team is then empty, which only EXT4_SIGNING=none accepts.
team="${EXT4_TEAM_ID:-$( (sed -n 's/^DEVELOPMENT_TEAM *= *//p' "${macos}/Config/Local.xcconfig" 2>/dev/null || true) | tail -1)}"

fail() {
    echo "error: $*" >&2
    exit 1
}

case "${signing}" in
    automatic | manual | none) ;;
    *) fail "EXT4_SIGNING must be automatic, manual, or none" ;;
esac
[[ -n "${version}" ]] || fail "no version in Cargo.toml"
if [[ "${signing}" != none && -z "${team}" ]]; then
    fail "set EXT4_TEAM_ID, or DEVELOPMENT_TEAM in macos/Config/Local.xcconfig"
fi
if [[ "${EXT4_REQUIRE_SIGNED:-}" == 1 && "${signing}" == none ]]; then
    fail "EXT4_REQUIRE_SIGNED=1 needs EXT4_SIGNING=automatic or manual"
fi

rm -rf "${work}"
mkdir -p "${work}" "${dist}"
# Keep Spotlight out: it registers every app it finds with LaunchServices
# (see the cleanup at the end).
touch "${work}/.metadata_never_index"
xcodegen_bin="$(command -v xcodegen || echo /opt/homebrew/bin/xcodegen)"
"${xcodegen_bin}" generate --spec "${macos}/project.yml" --project "${macos}" >/dev/null

version_settings=(MARKETING_VERSION="${version}" CURRENT_PROJECT_VERSION="${build_number}")
# Only automatic signing may let Xcode change the account (profiles,
# certificates), and only through the Xcode account on this Mac.
provisioning=(-allowProvisioningUpdates)
archive="${work}/Ext4.xcarchive"

echo "==> Building Ext4 ${version} (${build_number}), signing: ${signing}"
case "${signing}" in
    none)
        xcodebuild -project "${macos}/Ext4.xcodeproj" -scheme Ext4 -configuration Release -derivedDataPath "${work}/DerivedData" \
            -archivePath "${archive}" "${version_settings[@]}" \
            CODE_SIGNING_ALLOWED=NO CODE_SIGN_IDENTITY= DEVELOPMENT_TEAM= archive -quiet
        app="${archive}/Products/Applications/Ext4.app"
        ;;
    automatic)
        xcodebuild -project "${macos}/Ext4.xcodeproj" -scheme Ext4 -configuration Release -derivedDataPath "${work}/DerivedData" \
            -archivePath "${archive}" "${version_settings[@]}" DEVELOPMENT_TEAM="${team}" \
            "${provisioning[@]}" archive -quiet
        cat > "${work}/export.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>method</key><string>developer-id</string>
<key>teamID</key><string>${team}</string>
<key>signingStyle</key><string>automatic</string>
</dict></plist>
EOF
        xcodebuild -exportArchive -archivePath "${archive}" -exportOptionsPlist "${work}/export.plist" \
            -exportPath "${work}/export" "${provisioning[@]}" -quiet
        app="${work}/export/Ext4.app"
        ;;
    manual)
        profile="${EXT4_FSMODULE_PROFILE:-}"
        [[ -f "${profile}" ]] || fail "EXT4_FSMODULE_PROFILE must name the Developer ID provisioning profile of the FSKit module"
        security cms -D -i "${profile}" > "${work}/profile.plist"
        profile_uuid="$(/usr/libexec/PlistBuddy -c 'Print :UUID' "${work}/profile.plist")"
        profile_team="$(/usr/libexec/PlistBuddy -c 'Print :TeamIdentifier:0' "${work}/profile.plist")"
        [[ "${profile_team}" == "${team}" ]] || fail "the profile belongs to team ${profile_team}, not ${team}"
        /usr/libexec/PlistBuddy -c 'Print :Entitlements:com.apple.developer.fskit.fsmodule' "${work}/profile.plist" >/dev/null 2>&1 \
            || fail "the profile does not grant com.apple.developer.fskit.fsmodule"
        bundle_prefix="$(sed -n 's/^EXT4_BUNDLE_PREFIX *= *//p' "${macos}/Config/Base.xcconfig" | head -1)"
        for dir in "${HOME}/Library/Developer/Xcode/UserData/Provisioning Profiles" "${HOME}/Library/MobileDevice/Provisioning Profiles"; do
            mkdir -p "${dir}"
            cp "${profile}" "${dir}/${profile_uuid}.provisionprofile"
        done
        xcodebuild -project "${macos}/Ext4.xcodeproj" -scheme Ext4 -configuration Release -derivedDataPath "${work}/DerivedData" \
            -archivePath "${archive}" "${version_settings[@]}" DEVELOPMENT_TEAM="${team}" \
            CODE_SIGN_STYLE=Manual CODE_SIGN_IDENTITY="Developer ID Application" \
            EXT4_FSMODULE_PROFILE_SPECIFIER="${profile_uuid}" archive -quiet
        cat > "${work}/export.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>method</key><string>developer-id</string>
<key>teamID</key><string>${team}</string>
<key>signingStyle</key><string>manual</string>
<key>signingCertificate</key><string>Developer ID Application</string>
<key>provisioningProfiles</key><dict>
<key>${bundle_prefix}.fsmodule</key><string>${profile_uuid}</string>
</dict>
</dict></plist>
EOF
        xcodebuild -exportArchive -archivePath "${archive}" -exportOptionsPlist "${work}/export.plist" \
            -exportPath "${work}/export" -quiet
        app="${work}/export/Ext4.app"
        ;;
esac
[[ -d "${app}" ]] || fail "the build did not produce ${app}"

app_identity=""
if [[ "${signing}" != none ]]; then
    app_identity="$(codesign -dvv "${app}" 2>&1 | sed -n 's/^Authority=\(Developer ID Application:.*\)/\1/p' | head -1)"
    [[ -n "${app_identity}" ]] || fail "the exported app is not signed with a Developer ID Application certificate"
fi

echo "==> Building the command-line tool (universal)"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --manifest-path "${repo_root}/Cargo.toml" --release -p ext4-cli --target "${target}" --quiet
done
cli="${work}/ext4"
lipo -create -output "${cli}" \
    "${repo_root}/target/aarch64-apple-darwin/release/ext4" \
    "${repo_root}/target/x86_64-apple-darwin/release/ext4"
sign() {
    if [[ -n "${app_identity}" ]]; then
        codesign --force --options runtime --timestamp --sign "${app_identity}" "$@"
    else
        codesign --force --sign - "$@"
    fi
}
sign "${cli}"

echo "==> Staging the payload"
root="${work}/root"
mkdir -p "${root}/Applications" "${root}/Library/Filesystems" "${root}/Library/LaunchAgents" "${root}/usr/local/bin"
ditto "${app}" "${root}/Applications/Ext4.app"
ditto "${macos}/Filesystems/ext4.fs" "${root}/Library/Filesystems/ext4.fs"
# Disk Utility's formatter: the CLI acts as newfs when named newfs_ext4.
mkdir -p "${root}/Library/Filesystems/ext4.fs/Contents/Resources"
install -m 0755 "${cli}" "${root}/Library/Filesystems/ext4.fs/Contents/Resources/newfs_ext4"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString ${version}" -c "Set :CFBundleVersion ${build_number}" \
    "${root}/Library/Filesystems/ext4.fs/Contents/Info.plist"
sign "${root}/Library/Filesystems/ext4.fs"
install -m 0755 "${cli}" "${root}/usr/local/bin/ext4"
install -m 0644 "${macos}/pkg/LaunchAgents/dev.watzon.macext4.mount.plist" "${root}/Library/LaunchAgents/"

# Without this, Installer may "relocate" the app to any other copy with the
# same bundle ID it finds (for example a development build) instead of
# installing into /Applications.
pkgbuild --analyze --root "${root}" "${work}/component.plist" >/dev/null
i=0
while /usr/libexec/PlistBuddy -c "Print :${i}" "${work}/component.plist" >/dev/null 2>&1; do
    /usr/libexec/PlistBuddy -c "Delete :${i}:BundleIsRelocatable" "${work}/component.plist" 2>/dev/null || true
    /usr/libexec/PlistBuddy -c "Add :${i}:BundleIsRelocatable bool false" "${work}/component.plist"
    # Always replace the bundles: the app, the module, and the formatter
    # must match. With version checks, Installer keeps an installed bundle
    # whose version looks newer, and the install silently does nothing.
    /usr/libexec/PlistBuddy -c "Delete :${i}:BundleIsVersionChecked" "${work}/component.plist" 2>/dev/null || true
    /usr/libexec/PlistBuddy -c "Add :${i}:BundleIsVersionChecked bool false" "${work}/component.plist"
    i=$((i + 1))
done

echo "==> Building the installer"
pkgbuild --root "${root}" --component-plist "${work}/component.plist" \
    --scripts "${macos}/pkg/scripts" --identifier dev.watzon.macext4 --version "${version}" \
    --install-location / "${work}/Ext4-component.pkg" >/dev/null
sed "s/__VERSION__/${version}/" "${macos}/pkg/Distribution.xml" > "${work}/Distribution.xml"

output="${dist}/Ext4-${version}.pkg"
installer_identity=""
if [[ "${signing}" != none ]]; then
    installer_identity="${INSTALLER_IDENTITY:-$(security find-identity -v | sed -n "s/.*\"\(Developer ID Installer: .*(${team})\)\"/\1/p" | head -1)}"
fi
sign_args=()
if [[ -n "${installer_identity}" ]]; then
    sign_args=(--sign "${installer_identity}" --timestamp)
elif [[ "${EXT4_REQUIRE_SIGNED:-}" == 1 ]]; then
    fail "no 'Developer ID Installer' identity for team ${team}"
elif [[ "${signing}" != none ]]; then
    echo "warning: no 'Developer ID Installer' identity for team ${team}; the pkg is unsigned." >&2
    echo "         Create one in Xcode > Settings > Accounts > Manage Certificates." >&2
fi
productbuild --distribution "${work}/Distribution.xml" --resources "${macos}/pkg/resources" \
    --package-path "${work}" ${sign_args[@]+"${sign_args[@]}"} "${output}" >/dev/null

notary_auth=()
if [[ -n "${APPLE_API_KEY_PATH:-}" ]]; then
    notary_auth=(--key "${APPLE_API_KEY_PATH}" --key-id "${APPLE_API_KEY_ID:?}" --issuer "${APPLE_API_ISSUER_ID:?}")
elif [[ -n "${NOTARY_PROFILE:-}" ]]; then
    notary_auth=(--keychain-profile "${NOTARY_PROFILE}")
fi
if [[ ${#notary_auth[@]} -gt 0 ]]; then
    [[ -n "${installer_identity}" ]] || fail "notarization needs a signed pkg"
    echo "==> Notarizing"
    result="${work}/notarization.json"
    # notarytool can exit 0 for a rejected upload, so read the status.
    xcrun notarytool submit "${output}" "${notary_auth[@]}" --wait --output-format json > "${result}" || true
    status="$(plutil -extract status raw -o - "${result}" 2>/dev/null || true)"
    if [[ "${status}" != Accepted ]]; then
        cat "${result}" >&2
        fail "notarization status is '${status:-unknown}'. Read the log with: xcrun notarytool log <id> <credentials>"
    fi
    xcrun stapler staple "${output}"
elif [[ "${EXT4_REQUIRE_SIGNED:-}" == 1 ]]; then
    fail "no notarization credentials (APPLE_API_KEY_* or NOTARY_PROFILE)"
elif [[ "${signing}" != none ]]; then
    echo "note: set NOTARY_PROFILE or APPLE_API_KEY_* to notarize; without it, Gatekeeper blocks the pkg on other Macs." >&2
fi

# LaunchServices registers every Ext4.app the build makes. fskit_agent can
# then start the module from one of these copies, and RunningBoard refuses
# that ("Client not authorized"), so the installed module never runs.
# Unregister the build copies and delete them.
# Delete first, then unregister: a copy that still exists can be registered
# again in the meantime.
lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
# The archive step's copy is gone by now but still registered, so list the
# known build locations as well as what is on disk.
copies=(
    "${work}/DerivedData/Build/Intermediates.noindex/ArchiveIntermediates/Ext4/InstallationBuildProductsLocation/Applications/Ext4.app"
    "${archive}/Products/Applications/Ext4.app"
    "${work}/export/Ext4.app"
    "${root}/Applications/Ext4.app"
)
while IFS= read -r copy; do copies+=("${copy}"); done < <(find "${work}" -name Ext4.app -prune 2>/dev/null)
rm -rf "${archive}" "${work}/export" "${work}/DerivedData" "${root}"
for copy in "${copies[@]}"; do
    "${lsregister}" -u "${copy}" 2>/dev/null || true
done

(cd "${dist}" && shasum -a 256 "$(basename "${output}")" > "$(basename "${output}").sha256")
echo "==> ${output}"
