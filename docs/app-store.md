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

## Store listing

- Name: WikiSync by Plow · Subtitle: Your wiki on every Mac
- Category: Productivity · Price: free
- Support URL: https://plow.co · Privacy policy: https://plow.co/privacy
- Keywords: obsidian,sync,notes,wiki,markdown,peer to peer,encrypted,vault,p2p,private
- Screenshots: the three welcome steps, 1280×800.

Description:

> WikiSync keeps a folder of notes, such as your Obsidian vault, the same on
> every Mac you use. Edit a note on one machine and it's on the others moments
> later.
>
> Private by design
> • End-to-end encrypted: only your machines hold the keys.
> • Peer to peer: your Macs sync with each other directly. When they can't
>   reach each other, a relay passes along encrypted data it can't read.
> • No cloud copy: your notes and keys never touch a Plow server.
>
> Simple to set up
> Choose your wiki folder, then invite another Mac with a one-time link.
> WikiSync lives in the menu bar, shows each machine and when it last synced,
> and lists any conflicting edits for you to open.

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

Open at login is opt-in (guideline 2.4.5(iii)): a checkbox on the welcome's
last step, unchecked by default, and the menu's Open at Login toggle. The app
never turns it on by itself.

Export compliance: `Info.plist` sets `ITSAppUsesNonExemptEncryption` to false.
The app uses only standard encryption, TLS 1.3 over QUIC (iroh) and the
operating system's cryptography, so App Store Connect doesn't ask on each build.
