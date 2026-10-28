# Releasing

A release is a GitHub release with one installer, `Ext4-<version>.pkg`, and its
SHA-256 file. The Release workflow (`.github/workflows/release.yml`) builds,
signs, and notarizes the installer and attaches it to the release.

## One-time setup

The workflow needs seven repository secrets. One more is optional.

| Secret | Contents |
|---|---|
| `MACOS_CERTIFICATE_P12` | A `.p12` file, in base64, with the **Developer ID Application** certificate and its private key |
| `MACOS_INSTALLER_CERTIFICATE_P12` | A `.p12` file, in base64, with the **Developer ID Installer** certificate and its private key |
| `MACOS_CERTIFICATE_PASSWORD` | The password of the `.p12` files |
| `MACOS_INSTALLER_CERTIFICATE_PASSWORD` | Optional. The password of the installer `.p12` file, if it is different |
| `APPLE_API_KEY_P8` | An App Store Connect API key (`AuthKey_XXXX.p8`) |
| `APPLE_API_KEY_ID` | The key ID of that key |
| `APPLE_API_ISSUER_ID` | The issuer ID of the App Store Connect team |
| `EXT4_FSMODULE_PROFILE` | The Developer ID provisioning profile of the FSKit module, in base64 |

To get the files:

1. **Certificates.** In Xcode, open Settings > Accounts > Manage Certificates.
   Add a Developer ID Application and a Developer ID Installer certificate if
   you do not have them. Then, in Keychain Access > My Certificates, export
   each certificate (with its private key) to its own `.p12` file. Only the
   Account Holder can make a Developer ID Installer certificate.
2. **API key.** In App Store Connect, open Users and Access > Integrations >
   App Store Connect API and make a team key. The workflow uses it only to
   notarize, so the **Developer** role is sufficient. Apple lets you download
   the `.p8` file one time.
3. **Provisioning profile.** The FSKit module has a restricted entitlement
   (`com.apple.developer.fskit.fsmodule`), so it needs a provisioning
   profile. In the developer portal, open Certificates, Identifiers &
   Profiles > Profiles > **+** > **Developer ID**, select the App ID
   `dev.watzon.macext4.fsmodule` and the **same** Developer ID Application
   certificate as in `MACOS_CERTIFICATE_P12`, and download the profile.
   The workflow signs manually with it, so Xcode never changes the developer
   account from CI. (Automatic signing on a runner would make a new
   certificate on each run, and the runner deletes its private key.)

Then store the secrets with:

```sh
scripts/set-release-secrets.sh application.p12 installer.p12 AuthKey_XXXX.p8 <key id> <issuer id> macext4.provisionprofile
```

The script checks the files and the password before it sends anything to
GitHub. If your team ID is not `MB5789APU7`, set the repository variable
`EXT4_TEAM_ID`.

## Making a release

1. Set the new version in `Cargo.toml` (`[workspace.package] version`) and
   commit.
2. Make a GitHub release for the tag `v<version>`, and mark it as a
   **prerelease**:

   ```sh
   gh release create v0.2.0 --prerelease --generate-notes
   ```

3. The workflow starts. It stops if the tag does not agree with
   `Cargo.toml`, or if a secret is missing.
4. When the installer is attached, the workflow changes the release to a
   normal release and makes it the latest release, unless a newer version
   is already the latest.

If the workflow fails, fix the cause and run it again for the same tag:

```sh
gh workflow run release.yml -f tag=v0.2.0
```

The workflow changes only prereleases. After a release becomes a normal
release, make a new version instead.

## Building the installer on your Mac

```sh
scripts/package.sh
```

This uses the team in `macos/Config/Local.xcconfig` and the provisioning
profile that Xcode makes. Xcode must be signed in to the team. The script
signs the pkg when the keychain has a Developer ID Installer certificate,
and notarizes it when `NOTARY_PROFILE` names a `notarytool` keychain
profile:

```sh
xcrun notarytool store-credentials ext4-notary --apple-id <id> --team-id <team>
NOTARY_PROFILE=ext4-notary scripts/package.sh
```

`EXT4_SIGNING=none scripts/package.sh` builds an installer without
signatures. CI uses this to check the build. That installer does not work,
because macOS does not load an FSKit module without its provisioning
profile. The comment at the top of `scripts/package.sh` lists all settings.
