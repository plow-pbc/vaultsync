# vaultsync menu-bar app — short design

Product stage: internal tool for Sam's Macs first; App Store later.

## Intent
A Tailscale-like menu-bar app that shows, per synced folder, whether it's in sync and which
machines are connected, and lets Sam add folders and invite machines. Obsidian stays the editor.
The app is only a front end: vaultsync stays a CLI/daemon underneath (Sam: "front end for the
tui, still a terminal app behind the scenes").

## Decided with Sam
- Signed (Developer ID, notarized) download first; Mac App Store later.
- Team folders: any member machine may invite or remove (no admin roles).
- Menu, one section per folder:
  ```
  Sam's wiki · synced 2s ago
    ● Air (this Mac)  ● wakeup  ○ MBP, seen 3h ago
  plow-corporate-wiki · 1 conflict
    ● Air  ● Daniel's Mac
  Add a folder… · Invite a machine… (copies a plow.co/sync link) · Open in Obsidian
  ```
  Clicking a conflict opens that note.

## Approaches
1. **Native Swift menu-bar app (MenuBarExtra) driving the CLI — picked (my recommendation; Sam said build).**
2. Tauri app like Plow Chat — rejected: web view is heavy for a menu-bar item.
3. Tray icon inside the Rust CLI — rejected: non-native, hardest App Store path.

## Builder's calls
- CLI gains `vaultsync status --json` (per vault: folder, machine, head, pending count, peers with
  online/last-seen, last sync time, conflict files) — the daemon writes a small state file it reads.
- Multiple folders: one background service that runs every vault listed in a config
  (`~/.config/vaultsync/vaults`), replacing the single hard-coded `vaultsync` service; install.sh and
  the existing wiki install migrate to it without re-joining.
- App: SwiftUI MenuBarExtra, polls `status --json` every few seconds; "Add a folder" runs init or
  join (paste a code/link); "Invite" runs `vaultsync invite` and copies the plow.co/sync link;
  conflicts open via `obsidian://` (fallback: default app).
- Signing/notarization with the existing Apple team on mbp (arm64 + x86_64 universal build).
- Never touch Sam's real ~/Plow/wiki during development except through the normal service; test on
  scratch vaults.

## Done when
The app is installed on Sam's Air, shows Sam's wiki with its three machines and their state, and
an "Invite a machine" link joins a scratch test machine; screenshot of the menu.
