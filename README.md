# vaultsync

A command-line tool that keeps one folder of Markdown notes in sync across several machines,
peer to peer and end-to-end encrypted, with no server holding the content or the keys. Each
machine keeps the folder's full history in a git repository in its hidden `.vaultsync` folder
(vaults from 0.2 and earlier, which kept it beside the folder as `<folder>.git`, move it there on
first use), so every change can be
rolled back, concurrent edits merge line by line, and only same-line edits get git-style
conflict markers. Design: [`docs/superpowers/specs/2026-11-15-p2p-vault-sync-design.md`](docs/superpowers/specs/2026-11-15-p2p-vault-sync-design.md).
