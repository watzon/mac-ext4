#!/usr/bin/env bash
# Store the signing secrets of the Release workflow in the GitHub repository.
#
#   scripts/set-release-secrets.sh <application.p12> <installer.p12> <AuthKey.p8> <key id> <issuer id> <profile>
#
# application.p12 holds the Developer ID Application certificate and its
# private key; installer.p12 holds the Developer ID Installer certificate
# and its key. Both files must use the same password. profile is the
# manually managed Developer ID provisioning profile of the FSKit module. docs/releasing.md tells you how to get each file. The script sends
# the values only to GitHub, through `gh secret set`.
#
# Set EXT4_P12_PASSWORD to pass the password without a prompt.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ $# -eq 6 ]] || { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 1; }
app_p12=$1 installer_p12=$2 p8=$3 key_id=$4 issuer_id=$5 profile=$6
for f in "$app_p12" "$installer_p12"; do
    [[ -f "$f" ]] || { echo "error: no file at $f" >&2; exit 1; }
done
grep -q "BEGIN PRIVATE KEY" "$p8" 2>/dev/null || { echo "error: $p8 is not an App Store Connect API key (.p8)" >&2; exit 1; }
security cms -D -i "$profile" 2>/dev/null | grep -q com.apple.developer.fskit.fsmodule \
    || { echo "error: $profile is not a provisioning profile with the FSKit Module capability" >&2; exit 1; }

password=${EXT4_P12_PASSWORD:-}
if [[ -z "$password" ]]; then
    read -r -s -p "Password of the .p12 files: " password; echo
fi

# Fail now, not in the workflow, if the password is wrong or a file holds
# the wrong certificate.
check_p12() {
    local file=$1 kind=$2 subjects
    subjects=$(openssl pkcs12 -in "$file" -passin "pass:$password" -nokeys -legacy 2>/dev/null \
        || openssl pkcs12 -in "$file" -passin "pass:$password" -nokeys 2>/dev/null) \
        || { echo "error: cannot read $file with that password" >&2; exit 1; }
    grep -q "subject=.*$kind" <<< "$subjects" \
        || { echo "error: $file holds no $kind certificate" >&2; exit 1; }
    grep 'subject=' <<< "$subjects" | sed 's/^/certificate: /'
}
check_p12 "$app_p12" "Developer ID Application"
check_p12 "$installer_p12" "Developer ID Installer"

base64 < "$app_p12" | gh secret set MACOS_CERTIFICATE_P12
base64 < "$installer_p12" | gh secret set MACOS_INSTALLER_CERTIFICATE_P12
printf '%s' "$password" | gh secret set MACOS_CERTIFICATE_PASSWORD
gh secret set APPLE_API_KEY_P8 < "$p8"
printf '%s' "$key_id" | gh secret set APPLE_API_KEY_ID
printf '%s' "$issuer_id" | gh secret set APPLE_API_ISSUER_ID
base64 < "$profile" | gh secret set EXT4_FSMODULE_PROFILE
echo "The secrets are set:"
gh secret list
