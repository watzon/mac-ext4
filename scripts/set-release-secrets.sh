#!/usr/bin/env bash
# Store the signing secrets of the Release workflow in the GitHub repository.
#
#   scripts/set-release-secrets.sh <certificates.p12> <AuthKey.p8> <key id> <issuer id> [profile]
#
# certificates.p12 holds the Developer ID Application and the Developer ID
# Installer certificates with their private keys. profile is an optional,
# manually managed Developer ID provisioning profile of the FSKit module.
# docs/releasing.md tells you how to get each file. The script sends the
# values only to GitHub, through `gh secret set`.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ $# -eq 4 || $# -eq 5 ]] || { sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 1; }
p12=$1 p8=$2 key_id=$3 issuer_id=$4 profile=${5:-}
[[ -f "$p12" ]] || { echo "error: no file at $p12" >&2; exit 1; }
grep -q "BEGIN PRIVATE KEY" "$p8" 2>/dev/null || { echo "error: $p8 is not an App Store Connect API key (.p8)" >&2; exit 1; }
if [[ -n "$profile" ]]; then
    security cms -D -i "$profile" 2>/dev/null | grep -q com.apple.developer.fskit.fsmodule \
        || { echo "error: $profile is not a provisioning profile with the FSKit Module capability" >&2; exit 1; }
fi

read -r -s -p "Password of the .p12 file: " password; echo
# Fail now, not in the workflow, if the password is wrong or a certificate is missing.
subjects=$(openssl pkcs12 -in "$p12" -passin "pass:$password" -nokeys -legacy 2>/dev/null \
    || openssl pkcs12 -in "$p12" -passin "pass:$password" -nokeys 2>/dev/null) \
    || { echo "error: cannot read $p12 with that password" >&2; exit 1; }
for kind in "Developer ID Application" "Developer ID Installer"; do
    grep -q "subject=.*$kind" <<< "$subjects" \
        || { echo "error: $p12 holds no $kind certificate. Export both certificates into one file." >&2; exit 1; }
done
grep 'subject=' <<< "$subjects" | sed 's/^/certificate: /'

base64 < "$p12" | gh secret set MACOS_CERTIFICATE_P12
printf '%s' "$password" | gh secret set MACOS_CERTIFICATE_PASSWORD
gh secret set APPLE_API_KEY_P8 < "$p8"
printf '%s' "$key_id" | gh secret set APPLE_API_KEY_ID
printf '%s' "$issuer_id" | gh secret set APPLE_API_ISSUER_ID
if [[ -n "$profile" ]]; then
    base64 < "$profile" | gh secret set EXT4_FSMODULE_PROFILE
fi
echo "The secrets are set:"
gh secret list
