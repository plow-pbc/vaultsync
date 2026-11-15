use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, SecretKey};

use crate::git::{Git, exec};
use crate::members::{self, MEMBERS_PATH, Members};
use crate::merge::merge;

const MACHINE_KEY: &str = "vaultsync.machine";
/// Tries before giving up on landing a sync over files that keep changing.
const APPLY_TRIES: usize = 5;

/// A folder of notes whose git directory sits beside it (`<folder>.git`), so
/// editors and agents never see `.git`.
#[derive(Clone)]
pub struct Vault {
    git: Git,
}

fn git_dir_for(folder: &Path) -> PathBuf {
    let mut dir = folder.as_os_str().to_owned();
    dir.push(".git");
    dir.into()
}

/// Where a fetched peer head is kept.
fn peer_ref(peer: &EndpointId) -> String {
    format!("refs/vaultsync/peers/{peer}")
}

impl Vault {
    fn at(folder: PathBuf) -> Vault {
        Vault {
            git: Git {
                git_dir: git_dir_for(&folder),
                work_tree: folder,
            },
        }
    }

    pub fn folder(&self) -> &Path {
        &self.git.work_tree
    }

    /// This tool's own files inside the git directory.
    fn state(&self, name: &str) -> Result<PathBuf> {
        let dir = self.git.git_dir.join("vaultsync");
        fs::create_dir_all(&dir)?;
        Ok(dir.join(name))
    }

    /// Makes `folder` a vault. Existing history is adopted untouched: a `.git`
    /// inside the folder is moved beside it, and one already beside it is used
    /// as is. A folder with no history gets a first commit of its contents.
    pub fn init(folder: &Path, machine: Option<String>) -> Result<Vault> {
        fs::create_dir_all(folder)?;
        let vault = Vault::at(folder.canonicalize()?);
        let inner = vault.folder().join(".git");
        let beside = &vault.git.git_dir;
        // Check existing history before moving anything.
        for git_dir in [&inner, beside].into_iter().filter(|d| d.is_dir()) {
            let git = Git {
                git_dir: git_dir.clone(),
                work_tree: vault.folder().to_owned(),
            };
            git.run(["symbolic-ref", "-q", "HEAD"])
                .with_context(|| format!("HEAD in {} must be on a branch", git_dir.display()))?;
        }
        match (fs::symlink_metadata(&inner).ok(), beside.exists()) {
            (Some(meta), false) if meta.is_dir() => {
                fs::rename(&inner, beside).context("moving .git beside the folder")?
            }
            (Some(_), _) => bail!(
                "{} has its own .git and {} also exists; refusing to choose between them",
                vault.folder().display(),
                beside.display()
            ),
            (None, true) => {}
            (None, false) => {
                vault.git.run(["init", "-q", "-b", "main"])?;
            }
        }
        let fresh = vault.head().is_err();
        // A bare snapshot has no index; build it from HEAD without touching refs.
        if !fresh && !beside.join("index").exists() {
            vault.git.run(["read-tree", "HEAD"])?;
        }
        let machine = match machine {
            Some(m) => m,
            None => gethostname::gethostname().to_string_lossy().into_owned(),
        };
        vault.git.run(["config", MACHINE_KEY, &machine])?;
        vault.ensure_union_merge()?;
        if fresh {
            vault.git.run(["add", "-A"])?;
            vault.commit("Initialize vault")?;
        }
        Ok(vault)
    }

    /// Finds the vault containing `start`.
    pub fn open(start: &Path) -> Result<Vault> {
        let start = start.canonicalize()?;
        for dir in start.ancestors() {
            let vault = Vault::at(dir.to_owned());
            if vault.git.git_dir.is_dir() && vault.machine().is_ok() {
                vault.ensure_union_merge()?;
                return Ok(vault);
            }
        }
        bail!("no vault at or above {}", start.display())
    }

    /// Concurrent member additions merge as a union, never as markers.
    fn ensure_union_merge(&self) -> Result<()> {
        let attributes = self.git.git_dir.join("info").join("attributes");
        let existing = fs::read_to_string(&attributes).unwrap_or_default();
        let mut rule = format!("{MEMBERS_PATH} merge=union\n");
        if !existing.lines().any(|line| line == rule.trim_end()) {
            if !existing.is_empty() && !existing.ends_with('\n') {
                rule.insert(0, '\n');
            }
            fs::create_dir_all(attributes.parent().expect("has a parent"))?;
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&attributes)?
                .write_all(rule.as_bytes())?;
        }
        Ok(())
    }

    fn machine(&self) -> Result<String> {
        self.git.value(["config", MACHINE_KEY])
    }

    pub fn head(&self) -> Result<String> {
        self.git.value(["rev-parse", "--verify", "-q", "HEAD"])
    }

    /// This machine's node key, created on first use and never printed.
    pub fn secret_key(&self) -> Result<SecretKey> {
        let path = self.state("key")?;
        if let Ok(bytes) = fs::read(&path) {
            let bytes: [u8; 32] = bytes.try_into().ok().context("corrupt node key")?;
            return Ok(SecretKey::from_bytes(&bytes));
        }
        let key = SecretKey::generate();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?
            .write_all(&key.to_bytes())?;
        Ok(key)
    }

    pub fn node_id(&self) -> Result<EndpointId> {
        Ok(self.secret_key()?.public())
    }

    /// The machines this one trusts: the member list at HEAD, plus itself.
    pub fn members(&self) -> Result<Members> {
        let mut members = members::at(&self.git, "HEAD")?.unwrap_or_default();
        members
            .listed
            .entry(self.node_id()?)
            .or_insert(self.machine()?);
        Ok(members)
    }

    /// The other members, which this machine syncs with.
    pub fn peers(&self) -> Result<Vec<EndpointId>> {
        let me = self.node_id()?;
        let members = self.members()?;
        Ok(members
            .active()
            .map(|(id, _)| *id)
            .filter(|id| *id != me)
            .collect())
    }

    /// Adds a machine to the member list, in a commit signed by this one.
    pub fn add_member(&self, id: EndpointId, name: &str) -> Result<String> {
        self.change_members(&format!("Add member {name}"), |members| {
            if members.removed.contains_key(&id) {
                bail!("{name} was removed; it can only come back by joining with a fresh invite");
            }
            members.listed.insert(id, name.to_owned());
            Ok(())
        })
    }

    /// Removes a machine, by node id or name. Every member refuses it once the
    /// removal reaches them, and it can never be listed again.
    pub fn remove_member(&self, device: &str) -> Result<String> {
        let me = self.node_id()?;
        let members = self.members()?;
        let found: Vec<(EndpointId, String)> = members
            .active()
            .filter(|(id, name)| id.to_string() == device || name.as_str() == device)
            .map(|(id, name)| (*id, name.clone()))
            .collect();
        let [(id, name)] = &found[..] else {
            bail!("{device} names {} members; use its node id", found.len());
        };
        if *id == me {
            bail!("this machine can't remove itself; remove it from another member");
        }
        self.change_members(&format!("Remove member {name}"), |members| {
            members.listed.remove(id);
            members.removed.insert(*id, name.clone());
            Ok(())
        })
    }

    /// Edits the member list and commits it, signed by this machine.
    fn change_members(
        &self,
        subject: &str,
        edit: impl FnOnce(&mut Members) -> Result<()>,
    ) -> Result<String> {
        let _lock = self.lock()?;
        self.commit_pending_locked()?;
        let mut members = self.members()?;
        edit(&mut members)?;
        let path = self.folder().join(MEMBERS_PATH);
        let text = members::format(&members);
        if fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            return self.git.value(["rev-parse", "--short", "HEAD"]);
        }
        let dir = path.parent().expect("has a parent");
        // Never write through a symlink a synced commit put there.
        for p in [dir, &path] {
            if fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
                bail!(
                    "{} is a symlink; refusing to write the member list through it",
                    p.display()
                );
            }
        }
        fs::create_dir_all(dir)?;
        fs::write(&path, text)?;
        self.git.run(["add", MEMBERS_PATH])?;
        self.commit(subject)
    }

    /// Records a one-time invite valid for `ttl` and returns its secret. Only
    /// a hash of the secret is kept here.
    pub fn create_invite(&self, ttl: Duration) -> Result<[u8; 32]> {
        let secret: [u8; 32] = rand::random();
        let expires = SystemTime::now() + ttl;
        let secs = expires.duration_since(UNIX_EPOCH)?.as_secs();
        fs::write(self.invite_path(&secret)?, secs.to_string())?;
        Ok(secret)
    }

    /// Uses up an invite: a second use, an unknown one or an expired one fails.
    pub fn consume_invite(&self, secret: &[u8; 32]) -> Result<()> {
        let path = self.invite_path(secret)?;
        let expires = fs::read_to_string(&path).ok();
        // Removing it is the atomic claim: of two joins racing, one wins.
        if fs::remove_file(&path).is_err() {
            bail!("this invite code is unknown or was already used");
        }
        let expires: u64 = expires.unwrap_or_default().trim().parse().unwrap_or(0);
        if SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() > expires {
            bail!("this invite code has expired");
        }
        Ok(())
    }

    fn invite_path(&self, secret: &[u8; 32]) -> Result<PathBuf> {
        let dir = self.state("invites")?;
        fs::create_dir_all(&dir)?;
        Ok(dir.join(blake3::hash(secret).to_hex().as_str()))
    }

    /// An empty vault for a machine about to join: no commits yet. Refuses a
    /// folder that already has files or history beside it.
    pub fn for_join(folder: &Path, machine: Option<String>) -> Result<(Vault, bool)> {
        // Resolve `.` and symlinks first, so the history beside the folder is
        // checked where it really is.
        let folder = std::path::absolute(folder)?;
        let (Some(parent), Some(name)) = (folder.parent(), folder.file_name()) else {
            bail!("{} can't hold a vault", folder.display());
        };
        fs::create_dir_all(parent)?;
        let folder = folder
            .canonicalize()
            .or_else(|_| parent.canonicalize().map(|p| p.join(name)))?;
        let has_files = fs::read_dir(&folder).is_ok_and(|mut entries| entries.next().is_some());
        if has_files || git_dir_for(&folder).exists() {
            bail!(
                "{} already has files or history; join into an empty or new folder (--folder)",
                folder.display()
            );
        }
        let created = !folder.exists();
        fs::create_dir_all(&folder)?;
        let vault = Vault::at(folder);
        vault.git.run(["init", "-q", "-b", "main"])?;
        let machine = match machine {
            Some(m) => m,
            None => gethostname::gethostname().to_string_lossy().into_owned(),
        };
        vault.git.run(["config", MACHINE_KEY, &machine])?;
        vault.ensure_union_merge()?;
        Ok((vault, created))
    }

    /// Takes the inviter's full history from its bundle and fills the folder.
    pub fn adopt(&self, bundle: &Path) -> Result<()> {
        let mut cmd =
            self.git
                .command(["fetch", "-q", "--update-head-ok", "--no-write-fetch-head"]);
        cmd.arg(bundle).arg("+HEAD:refs/heads/main");
        exec(cmd)?;
        fs::remove_file(bundle)?;
        self.git.run(["read-tree", "-u", "--reset", "HEAD"])?;
        Ok(())
    }

    /// Removes a vault [`for_join`](Self::for_join) made, after a failed join.
    pub fn discard(self, created_folder: bool) -> Result<()> {
        // Only what join wrote: files checked out and still as checked out,
        // their emptied folders, the history beside, and the folder if join
        // made it and nothing else landed there meanwhile.
        let listed = self.git.run(["ls-files", "-z"]).unwrap_or_default();
        let changed = self
            .git
            .run(["ls-files", "-z", "--modified"])
            .unwrap_or_default();
        let changed: Vec<&str> = changed.split('\0').collect();
        for path in listed
            .split('\0')
            .filter(|p| !p.is_empty() && !changed.contains(p))
        {
            let file = self.folder().join(path);
            if fs::remove_file(&file).is_ok() {
                for dir in file.ancestors().skip(1).take_while(|d| *d != self.folder()) {
                    if fs::remove_dir(dir).is_err() {
                        break;
                    }
                }
            }
        }
        fs::remove_dir_all(&self.git.git_dir)?;
        if created_folder {
            let _ = fs::remove_dir(self.folder());
        }
        Ok(())
    }

    pub fn machine_name(&self) -> Result<String> {
        self.machine()
    }

    /// Serializes everything that writes to the index, the work tree or HEAD,
    /// and finishes a sync that a crash interrupted.
    fn lock(&self) -> Result<File> {
        let file = File::create(self.git.git_dir.join("vaultsync.lock"))?;
        file.lock()?;
        self.recover()?;
        Ok(file)
    }

    /// Commits the staged changes as this machine, signed with its node key,
    /// plus the writing agent when `VAULTSYNC_AGENT` names one.
    fn commit(&self, subject: &str) -> Result<String> {
        let machine = self.machine()?;
        let email = format!("vaultsync@{machine}");
        let tree = self.git.value(["write-tree"])?;
        let parent = self.head().ok();
        let parents: Vec<&str> = parent.iter().map(String::as_str).collect();
        let mut message = format!("{subject}\n\n");
        if let Ok(agent) = std::env::var("VAULTSYNC_AGENT") {
            message.push_str(&format!("Agent: {agent}\n"));
        }
        message.push_str(&members::sign(&self.secret_key()?, &tree, &parents));
        let mut cmd = self.git.command(["commit-tree", &tree, "-m", &message]);
        for p in &parents {
            cmd.args(["-p", p]);
        }
        for (key, value) in [("NAME", &machine), ("EMAIL", &email)] {
            cmd.env(format!("GIT_AUTHOR_{key}"), value)
                .env(format!("GIT_COMMITTER_{key}"), value);
        }
        let id = exec(cmd)?.trim_end().to_owned();
        let mut update = vec!["update-ref", "HEAD", &id];
        update.extend(parents.first().copied());
        self.git.run(update)?;
        self.git.value(["rev-parse", "--short", &id])
    }

    /// Commits every change in the folder, if there is any.
    pub fn commit_pending(&self) -> Result<Option<String>> {
        let _lock = self.lock()?;
        self.commit_pending_locked()
    }

    fn commit_pending_locked(&self) -> Result<Option<String>> {
        self.git.run(["add", "-A"])?;
        let staged = self.git.run(["diff", "--cached", "--name-only", "-z"])?;
        let files: Vec<&str> = staged.split('\0').filter(|f| !f.is_empty()).collect();
        let subject = match files.as_slice() {
            [] => return Ok(None),
            [one] => format!("Update {one}"),
            many => format!("Update {} files", many.len()),
        };
        self.commit(&subject).map(Some)
    }

    pub fn status(&self) -> Result<String> {
        let pending = self.git.run(["status", "--porcelain", "-z"])?;
        let count = pending.split('\0').filter(|e| !e.is_empty()).count();
        Ok(format!(
            "vault:   {}\nhistory: {}\nmachine: {}\nnode:    {}\nbranch:  {}\nhead:    {}\n{count} uncommitted change{}\n",
            self.folder().display(),
            self.git.git_dir.display(),
            self.machine()?,
            self.node_id()?,
            self.git.value(["symbolic-ref", "--short", "HEAD"])?,
            self.git.value(["log", "-1", "--format=%h %s"])?,
            if count == 1 { "" } else { "s" },
        ))
    }

    pub fn log(&self, limit: usize, path: Option<&Path>) -> Result<String> {
        let mut args = vec![
            "log".into(),
            format!("-n{limit}"),
            "--date=format:%Y-%m-%d %H:%M:%S".into(),
            "--format=%h  %ad  %an  %s".into(),
        ];
        if let Some(path) = path {
            args.extend(["--".into(), path.display().to_string()]);
        }
        self.git.run(args)
    }

    /// Restores the whole vault, or one path, to its state at `rev` as a new
    /// commit. Pending edits are committed first, so nothing is lost.
    pub fn rollback(&self, rev: &str, path: Option<&Path>) -> Result<String> {
        let _lock = self.lock()?;
        // Resolve first: the checkpoint below moves HEAD, and with it `HEAD~1`.
        let id = self
            .git
            .value(["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?;
        let short = self.git.value(["rev-parse", "--short", &id])?;
        self.commit_pending_locked()?;
        let (mut tree, what) = match path {
            None => (
                self.git.value(["rev-parse", &format!("{id}^{{tree}}")])?,
                "vault".to_owned(),
            ),
            Some(path) => (
                self.tree_with("HEAD", &id, path)?,
                path.display().to_string(),
            ),
        };
        // A rollback never undoes a membership change, whatever it selects:
        // the member list stays as it is now.
        let members = Path::new(MEMBERS_PATH);
        if self.has(&tree, members) || self.has("HEAD", members) {
            tree = self.tree_with(&tree, "HEAD", members)?;
        }
        // Git overwrites ignored files freely; refuse instead.
        let added = self.git.run([
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=A",
            "HEAD",
            &tree,
        ])?;
        let ignored = self.git.run([
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
        ])?;
        let ignored: Vec<&str> = ignored.split('\0').filter(|f| !f.is_empty()).collect();
        // The same path, or one inside the other (a file where a folder was).
        let overlaps = |a: &str, b: &str| {
            a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
        };
        let clash: Vec<&str> = added
            .split('\0')
            .filter(|f| !f.is_empty() && ignored.iter().any(|i| overlaps(f, i)))
            .collect();
        if !clash.is_empty() {
            bail!(
                "rollback would overwrite untracked files: {}; move them aside",
                clash.join(", ")
            );
        }
        // One checked update for both modes: it refuses to overwrite a file
        // saved since the checkpoint.
        self.git
            .run(["read-tree", "-m", "-u", "HEAD", &tree])
            .context("files changed during rollback; try again")?;
        if self
            .git
            .run(["diff", "--cached", "--name-only"])?
            .is_empty()
        {
            bail!("{what} already matches {short}");
        }
        self.commit(&format!("Roll back {what} to {short}"))
    }

    /// HEAD's tree with `path` as it was at `id` (absent if it was absent),
    /// built in a scratch index so the real one is untouched.
    fn has(&self, rev: &str, path: &Path) -> bool {
        self.git
            .run(["cat-file", "-e", &format!("{rev}:{}", path.display())])
            .is_ok()
    }

    fn tree_with(&self, base: &str, id: &str, path: &Path) -> Result<String> {
        let index = self.git.git_dir.join("vaultsync-rollback-index");
        let with_index = |args: &[&OsStr]| {
            let mut cmd = self.git.command(args);
            cmd.env("GIT_INDEX_FILE", &index);
            exec(cmd)
        };
        let source = format!("--source={id}");
        with_index(&["read-tree".as_ref(), base.as_ref()])?;
        with_index(&[
            "restore".as_ref(),
            "--staged".as_ref(),
            source.as_ref(),
            "--".as_ref(),
            path.as_os_str(),
        ])?;
        let tree = with_index(&["write-tree".as_ref()])?;
        fs::remove_file(&index)?;
        Ok(tree.trim_end().to_owned())
    }

    /// Commit ids to tell a peer this machine already has.
    pub fn haves(&self) -> Result<Vec<String>> {
        let refs = self.git.run([
            "for-each-ref",
            "--format=%(objectname)",
            "refs/vaultsync/peers",
        ])?;
        let head = self.head()?;
        Ok(refs.lines().map(str::to_owned).chain([head]).collect())
    }

    /// The subset of `ids` that are commits here.
    pub fn present(&self, ids: &[String]) -> Result<Vec<String>> {
        let query: String = ids.iter().map(|id| format!("{id}^{{commit}}\n")).collect();
        let answers = self
            .git
            .run_stdin(["cat-file", "--batch-check=%(objectname)"], &query)?;
        Ok(ids
            .iter()
            .zip(answers.lines())
            .filter(|(_, a)| !a.ends_with(" missing"))
            .map(|(id, _)| id.clone())
            .collect())
    }

    /// Whether HEAD has commits that `haves` lack.
    pub fn has_news(&self, haves: &[String]) -> Result<bool> {
        let mut args = vec!["rev-list", "-n1", "HEAD", "--not"];
        args.extend(haves.iter().map(String::as_str));
        Ok(!self.git.run(args)?.is_empty())
    }

    /// `git bundle create - HEAD --not <haves>`, for streaming to a peer.
    pub fn bundle_command(&self, haves: &[String]) -> std::process::Command {
        let mut cmd = self.git.command(["bundle", "create", "-q", "-", "HEAD"]);
        if !haves.is_empty() {
            cmd.arg("--not").args(haves);
        }
        cmd
    }

    /// Path for a bundle arriving from `peer`.
    pub fn incoming(&self, peer: &EndpointId) -> Result<PathBuf> {
        // A fresh name per attempt: no two transfers ever share a file.
        static ATTEMPT: AtomicU64 = AtomicU64::new(0);
        let attempt = ATTEMPT.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        self.state(&format!(
            "incoming-{}-{pid}-{attempt}.bundle",
            peer.fmt_short()
        ))
    }

    /// Stores a peer's head from its bundle.
    pub fn receive(&self, peer: &EndpointId, bundle: &Path) -> Result<()> {
        let name = peer_ref(peer);
        let mut cmd = self.git.command(["fetch", "-q", "--no-write-fetch-head"]);
        cmd.arg(bundle).arg(format!("+HEAD:{name}"));
        let fetched = exec(cmd);
        fs::remove_file(bundle)?;
        fetched.map(drop)
    }

    /// Brings a peer's head into this vault: fast-forward, or a merge. Pending
    /// edits are committed first, and a file edited while the sync runs is
    /// never overwritten. Returns the new HEAD if it moved.
    pub fn integrate(&self, peer: &EndpointId) -> Result<Option<String>> {
        let _lock = self.lock()?;
        let Ok(tip) = self
            .git
            .value(["rev-parse", "-q", "--verify", &peer_ref(peer)])
        else {
            return Ok(None);
        };
        for _ in 0..APPLY_TRIES {
            self.commit_pending_locked()?;
            let head = self.head()?;
            if self.is_ancestor(&tip, &head) {
                return Ok(None);
            }
            members::validate(&self.git, &head, &tip, &self.members()?)
                .with_context(|| format!("refusing history from {}", peer.fmt_short()))?;
            // Merge only when each side has edits the other lacks. A head that
            // already holds all of ours is taken as is; merging it again is how
            // a ring of peers would keep re-merging the same edits forever.
            let ours_covered = self.edits_beyond(&head, &tip)? == 0;
            let theirs_covered = self.edits_beyond(&tip, &head)? == 0;
            let new = match (ours_covered, theirs_covered) {
                // Same edits, different merges: the greater id wins everywhere.
                (true, true) if head > tip => return Ok(None),
                (true, _) => tip.clone(),
                // We hold all of theirs and more; they take ours.
                (false, true) => return Ok(None),
                (false, false) => merge(&self.git, &head, &tip)?,
            };
            if self.apply(&head, &new)? {
                return Ok(Some(new));
            }
        }
        bail!("files kept changing; will retry")
    }

    /// Edits (non-merge commits) reachable from `a` but not from `b`.
    fn edits_beyond(&self, a: &str, b: &str) -> Result<usize> {
        Ok(self
            .git
            .value(["rev-list", "--no-merges", "--count", a, "--not", b])?
            .parse()?)
    }

    fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.git.run(["merge-base", "--is-ancestor", a, b]).is_ok()
    }

    /// Moves HEAD, index and work tree from `old` to `new`. False if a file
    /// git would change was edited meanwhile; nothing is touched then. The
    /// intent file records how far it got, for `recover`.
    fn apply(&self, old: &str, new: &str) -> Result<bool> {
        if hooked("pause-before-apply") {
            eprintln!("test hook: pausing before apply");
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        // git's check below treats a deleted tracked file as clean and would
        // write it back, so a rename made since the checkpoint would leave
        // both names. Any tracked change since then: commit it and merge again.
        if self.git.run(["diff", "--quiet"]).is_err() {
            return Ok(false);
        }
        let intent = self.state("apply")?;
        fs::write(&intent, format!("started {old} {new}\n"))?;
        crash_if_hooked("crash-before-worktree");
        if self.git.run(["read-tree", "-m", "-u", old, new]).is_err() {
            fs::remove_file(&intent)?;
            return Ok(false);
        }
        fs::write(&intent, format!("written {old} {new}\n"))?;
        crash_if_hooked("crash-after-worktree");
        self.git.run(["update-ref", "HEAD", new, old])?;
        fs::remove_file(&intent)?;
        Ok(true)
    }

    /// Settles an `apply` a crash interrupted, never undoing what a user did
    /// since. Once the work tree was fully written only HEAD is left to move,
    /// and any difference from `new` is the user's. Before that, git may have
    /// written part of `new`: HEAD stays at `old`, and whatever is on disk is
    /// committed as an edit and merged again.
    fn recover(&self) -> Result<()> {
        let intent = self.state("apply")?;
        let Ok(text) = fs::read_to_string(&intent) else {
            return Ok(());
        };
        let [phase, old, new] = text.split_whitespace().collect::<Vec<_>>()[..] else {
            bail!("corrupt apply intent: {text}");
        };
        if phase == "written" && self.head()? == old {
            self.git.run(["update-ref", "HEAD", new, old])?;
        }
        fs::remove_file(&intent)?;
        Ok(())
    }
}

/// Fault injection for tests: `VAULTSYNC_TEST_HOOK=<name>`.
pub fn hooked(name: &str) -> bool {
    std::env::var("VAULTSYNC_TEST_HOOK").as_deref() == Ok(name)
}

fn crash_if_hooked(name: &str) {
    if hooked(name) {
        std::process::exit(9);
    }
}
