# WikiSync on the Mac App Store

## Build and upload

```sh
APPSTORE=1 \
APPSTORE_IDENTITY="Apple Distribution: …" \
INSTALLER_IDENTITY="3rd Party Mac Developer Installer: …" \
PROVISIONING_PROFILE=/path/to/WikiSync_Mac_App_Store.provisionprofile \
macos/build.sh

xcrun altool --validate-app macos/build/WikiSync-by-Plow.pkg --api-key "$ASC_KEY_ID" --api-issuer "$ASC_ISSUER_ID"
xcrun altool --upload-package macos/build/WikiSync-by-Plow.pkg --api-key "$ASC_KEY_ID" --api-issuer "$ASC_ISSUER_ID" --wait
```

altool finds `AuthKey_<ASC_KEY_ID>.p8` in `~/.appstoreconnect/private_keys` or
`$API_PRIVATE_KEYS_DIR`. Each upload needs a higher version: `CFBundleVersion`
is the workspace version in `Cargo.toml`.

## For App Review

Entitlements (`macos/VaultSync.entitlements`), and why:

- `app-sandbox`: required.
- `files.user-selected.read-write`, `files.bookmarks.app-scope`: the app syncs
  folders the user picks in an open panel, and keeps access to them across
  launches.
- `network.client`: connects to the user's own other machines, directly or
  through a relay that only forwards encrypted data, to sync those folders.
- `network.server`: peer to peer, there is no server. The user's other machines
  connect to this Mac over QUIC, so the app listens for them.

Review notes to give: no account or sign-in. To try it, choose any folder in the
welcome window (or Add a folder… in the menu); the menu then lists it.
Invite a machine… copies a one-time link a second Mac joins with, which is how
sync between two machines can be seen.

App Privacy: no data collected, no tracking. `PrivacyInfo.xcprivacy` declares
the required-reason APIs the binary uses: UserDefaults (CA92.1), file
timestamps in user-picked folders and the container (3B52.1, C617.1), and
`mach_absolute_time` for elapsed time (35F9.1).

## Open before submitting

- Open at login: on first launch the app registers itself as a login item
  without asking. Guideline 2.4.5(iii) forbids starting at login without the
  user's consent, so this needs an opt-in (the menu already has the toggle).
- Export compliance: sync uses standard TLS 1.3 over QUIC (iroh) and ed25519
  keys. App Store Connect asks about encryption on each build until
  `ITSAppUsesNonExemptEncryption` is set in `Info.plist`; that answer is Plow's
  to give.
