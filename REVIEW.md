# Review instructions — vaultsync

Repo-specific reviewer policy. The universal review policy is prepended by the reviewer and is not restated here.

## What this repo is

A Rust CLI that keeps one folder of Markdown notes in sync across machines, peer to peer and end-to-end encrypted, with each machine holding the folder's full git history. No server holds content or keys.

**Stage:** early, syncing real notes.

## Review priority

**#1: low complexity.** Pre-PMF, simple code that meets the need beats complete code. Weigh every finding against this first.

- **Cut scope creep.** Flag a change that grows past its stated ask. The remedy is a smaller change, not more code.
- **DRY.** Flag a second copy of logic or facts that already have an owner. The remedy is to reuse that owner or delete the copy.
- **Few conditionals.** Each new branch, guard, fallback or special case is a finding unless it handles a failure that actually happens. Security and access checks are not special cases.
- **Model decision over regex.** Judging the meaning of natural language belongs to an LLM call, never a regex or keyword list. Deterministic checks on structured or model output (a required tag, a value from a fixed set, a sum) stay code and are not findings.

Prefer subtractive remedies (delete, collapse, reuse) over additive ones at any severity.

## Where the real risks are

- **Data loss.** A sync or merge path that can drop, overwrite or corrupt a note is Critical, however rare.
- **Encryption and key handling.** Flag plaintext or key material reaching a peer, a log or disk outside the vault's protected state. These are security checks, never special cases to cut.
