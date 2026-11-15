# P2P vault sync — design

Status: draft for Sam's review · Priority `!v6bJRp82DfyJ6OKzVaM6fLFU2oojPQBFA49e9zRAWCE/e6f37d2e`
Brainstormed with Sam over iMessage, 11/15. Security check: Knightwatch.
Product stage: **internal tool** (Sam's own machines first), built clean enough to open up later.

## Intent

Sam's agents run on several machines (the MacBook Air, the MacBook Pro, wakeup on Linux,
the Raspberry Pis). He wants them to share **one Obsidian-style knowledge base** — a folder
of Markdown notes, today `~/Plow/wiki` on the Air — that every agent on every machine can read
and write, kept in sync directly between the machines, end-to-end encrypted, with full edit
history and rollback.

Today the wiki lives only on the Air (318 files, 6.8 MB, 286 `.md`), its history in
`~/Plow/wiki.git` (pushed to GitHub as a snapshot). 128 of its last 130 commits are agents
(cos, str); one is Sam. Nothing syncs it to other machines.

## Goals

- Every machine in the vault converges on the same notes, without a central server holding
  the content or the keys.
- Full history on every machine; any change can be rolled back.
- Concurrent edits merge line by line; only when two machines changed the **same line** does
  the file get git-style conflict markers, which agents (or Sam) resolve.
- Adding a machine is one link and one command; the encryption keys never touch a Plow server.
- Runs on macOS (arm64, x86_64) and Linux (x86_64, arm64).

## Non-goals (for now)

- A GUI or Mac App Store app (later; build so it stays possible).
- Real-time co-editing (Dropbox Paper / CRDT style).
- Windows.
- Replacing the existing GitHub snapshot push of Sam's wiki — it stays as an off-site backup.

## Anti-goals (failure even if it "works")

- Any key or note content on a Plow server.
- Silent data loss: a concurrent edit that disappears instead of merging or showing markers.
- Scrambled notes (interleaved lines) that an agent would read as truth.
- Sam's existing wiki history lost in the move.

## Approaches considered

1. **git as the engine, carried peer-to-peer over iroh** — *picked (Sam, 11/15)*. Each machine
   holds a git repository of the vault; a daemon auto-commits local edits and exchanges commits
   with peers. git gives history, rollback and 3-way line merges with conflict markers for
   free, and the wiki is already a git repository.
2. File sync with a merge on top (Syncthing / Obsidian Sync style): simpler transport, but
   history and rollback have to be built, and Syncthing's answer to conflicts is a duplicate
   file. Rejected: reinvents what git already does.
3. Text CRDT (Automerge / Yjs): no markers ever, but ordinary editors and agents write plain
   files, so it degrades to diff-and-replay plus a metadata store, and concurrent edits can
   interleave. Rejected: the scrambled-note anti-goal.

## Decided with Sam

- Cross-platform **command-line tool**, installed by an `install.sh`; GUI later.
- Merge line by line; git-style markers only on same-line conflicts.
- git underneath for history and rollback.
- Joining follows magic-wormhole's pattern: a Plow link carries a one-time code after the `#`
  (browsers never send that part to the server); the page gives a `curl … install.sh`
  command; the new machine dials an existing member directly and is admitted by it. Plow never
  sees a key.
- Standalone: no Plow account needed to use it.

## Security (Knightwatch, 11/15)

- Transport: iroh (QUIC + TLS 1.3, each device identified by an ed25519 node ID); public n0
  relays as fallback carry only ciphertext.
- Each machine accepts only peers whose node IDs are on the vault's member list (pinned).
- The join code is one-time and expires within minutes.
- "Remove" drops a device from the member list; that is revocation.
- No app-level at-rest encryption; FileVault / disk encryption covers it.

## Builder's calls (marked as mine, Sam can override)

- **Language: Rust** — iroh is native Rust and latch (same team) already uses it.
- **No shared vault key.** Sam described "the existing computer passes its encryption key to the
  new one"; with per-device keys and a pinned member list, the existing machine instead adds
  the newcomer's node ID to the list and every member then trusts it. Same effect, nothing
  secret to copy, and revocation is just removal.
- The vault folder stays clean: the git directory lives beside it (as `~/Plow/wiki.git` does
  today), so editors and agents never see `.git`.
- Commits carry the machine name, and the writing agent's name when the tool can tell.
- Sync is continuous: a file watcher with a short debounce, plus a periodic catch-up with
  every reachable peer.
- Binary files (images, PDFs) that conflict keep both versions side by side rather than
  getting markers.
- Runs as a user service (launchd on macOS, `systemd --user` on Linux) installed by
  `install.sh`.
- **Merging is ordinary git three-way merge.** "Same line" means git's hunk rule in practice:
  edits close enough to overlap also get markers. No custom merge policy.
- **Conflicted files are committed with their markers and synced on**, so every machine
  converges at once and unrelated syncing never stalls; whoever fixes the markers commits the
  fix like any other edit.
- **Changes git can't mark** keep both sides, never silently dropping one: modify vs delete
  keeps the modified file; add vs add of a binary keeps both copies side by side; rename vs
  modify follows git's rename detection, otherwise both paths are kept.
- **Any member machine may invite or remove** (all are Sam's). If a remove and another change
  race, the remove wins.
- **Revocation takes effect as it reaches each member.** A member that is offline refuses the
  removed device once it next syncs with anyone; no central authority.
- **Merges are deterministic, so peers converge.** Any peer may merge, but a merge commit's
  parents are ordered by object id and its author, committer and timestamp are fixed
  (author "vault-sync", time = the later parent's time), so two peers merging the same pair
  produce the identical commit and stop. (Audit 11/15, Critical 1.)
- **Membership lives in the vault and is signed.** The member list is a file in the repo
  (`.vault/members`); a change to it counts only in a commit signed by a device that was a
  member at its parent, using that device's ed25519 key. Removal is monotonic: a removed node
  ID is tombstoned and only a fresh invite can re-add it. After removal, its new commits are
  rejected wherever the removal has arrived. (Audit Critical 2.)
- **Sync never clobbers a live edit.** The daemon merges off to the side and writes a file
  back only if it is unchanged since the merge began; otherwise the new edit is committed and
  merged again. One sync operation runs at a time, and an interrupted one resumes cleanly
  after a restart. (Audit Important 1.)
- **The GitHub snapshot is a trusted-content exception.** It stores Sam's notes on a private
  GitHub repo, as it does today; the end-to-end claim covers the sync itself. (Audit Important 5.)
- **A joining machine gets the vault at the same path as the machine that invited it**
  (`~/Plow/wiki` for Sam), overridable by a flag. If that folder already has files or git
  history, the installer stops and says so rather than merging or overwriting.

## Prior art we lean on (search 11/15)

Nothing permissively licensed does this end to end.
- **Dependencies:** iroh (Apache-2.0) for transport, node IDs and relays; git2 or the system
  `git` for commits, three-way merge and markers.
- **Fork/vendor:** git-remote-iroh (Apache-2.0, ~52 KB): runs real `upload-pack` /
  `receive-pack` over iroh. Replace its URL shared secret with the member-list check on the
  connecting node ID.
- **Inspiration only:** git-sync (the auto-commit loop's safety checks), sendme / dumbpipe
  (ticket format), obsidian-git; sukarfleet (AGPL, closest in spirit), magic-wormhole.rs (EUPL),
  Syncthing, Radicle (a forge model with per-device namespaces; more surface than it saves).
- **No PAKE:** the join code rides in the URL fragment, so it is a long random one-time
  secret plus the inviter's iroh ticket; no spake2, no rendezvous server.

## Left to the builder

Commit granularity and debounce, the wire format for exchanging commits (git bundles or
packfiles over an iroh stream), the CLI's exact command names, and the release pipeline.

## Chunks

### Global Constraints

- Product stage: internal tool on Sam's machines; size the review bar to that, not to a public
  launch.
- No key or note content ever sent to, stored on or logged by a Plow server.
- A concurrent edit is never dropped: it merges, or the file carries conflict markers.
- Never rewrite or lose existing git history of an imported vault.
- CI builds and runs the tests on macOS and Linux from Chunk 1; real-machine checks from Chunk 2 include at least one Mac and one Linux machine.
- Secrets (join codes, node keys) are never printed in full in logs or chat.
- New standalone repository; no worktrees; one cook per checkout.

### Chunk 1: Local engine
Implements: §Approaches (1), §Builder's calls (git beside the folder, commit attribution, watcher)
Interfaces: produces a local vault (folder + git dir) that later chunks sync; CLI `init`, `status`, `log`, `rollback`
Done when: test output shows an edit to a note becoming a commit within seconds, `log` listing it with machine name, and `rollback` restoring a prior version as a new commit; `init` on a copy of an existing repo keeps its history (every ref and commit id identical before and after); tests pass on macOS and Linux in CI.

### Chunk 2: Peer-to-peer sync between pinned machines
Implements: §Goals (converge, line merge, markers), §Security (transport, pinned IDs)
Interfaces: consumes Chunk 1's vault · produces the signed member list (`.vault/members`) and commit exchange over iroh
Done when: on three real machines (at least one Mac, one Linux) with node IDs added by hand: edits on any appear on all; distant-line edits merge cleanly; same-line edits leave the same marker-bearing file on all, and syncing continues after the markers are fixed; modify-vs-delete keeps the modified file, binary add-vs-add keeps both, rename-vs-modify keeps the content; full history arrives (ref and commit ids match); after going idle, all three have the identical head and tree and no new commits appear over repeated catch-up cycles; an edit made during a sync survives, including across a daemon restart mid-sync; a machine not on the list is refused (shown in logs).

### Chunk 3: Invite, join and remove
Implements: §Decided with Sam (join), §Security (one-time code, remove = revocation)
Interfaces: consumes Chunk 2's member list · produces `invite` (prints a link + code), `join <code>`, `remove <device>`
Done when: a third machine joins with a fresh code and receives the full vault and history; reusing that code, or using one past its expiry, is refused; after `remove`, the removed machine is refused by every member the removal has reached, including one that was offline at the time and synced afterwards; joining into a non-empty folder stops with a message.

### Chunk 4: install.sh, release builds, user service
Implements: §Decided with Sam (install.sh), §Builder's calls (user service)
Interfaces: consumes Chunks 1–3's binary · produces release artifacts per OS/arch and an `install.sh` that takes a join code
Done when: `install.sh` installs, starts the service and joins on the MacBook Air, MacBook Pro, wakeup and one Raspberry Pi; the service survives a reboot on one macOS and one Linux machine.

### Chunk 5: The Plow join page
Implements: §Decided with Sam (Plow link, code after `#`)
Interfaces: consumes Chunk 4's `install.sh` · produces a `plow.co/sync` page that turns the code in its URL fragment into the install command
Done when: opening a `plow.co/sync#<code>` link shows the right command; the server's access logs for that request contain no code.

### Chunk 6: Move Sam's wiki onto it
Implements: §Intent, §Anti-goals (history kept), §Non-goals (GitHub snapshot stays)
Interfaces: consumes Chunks 1–5
Done when: the Air's `~/Plow/wiki` is the vault's first member with all existing history intact (refs and commit ids recorded before the move match after it); wakeup and the MacBook Pro have joined; an agent on wakeup writes a note that an agent on the Air then reads; the GitHub snapshot push still runs.
