use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use git2::build::CheckoutBuilder;
use git2::{
    ErrorCode, Index, IndexAddOption, ObjectType, Oid, Reference, Repository,
    RepositoryInitOptions, RepositoryOpenFlags, Signature, Status, StatusOptions, Tree,
    TreeWalkMode, TreeWalkResult,
};
use iroh::{EndpointId, SecretKey};

use crate::members::{self, MEMBERS_PATH, Members};
use crate::merge::{index_entry, merge};

const MACHINE_KEY: &str = "vaultsync.machine";
/// Tries before giving up on landing a sync over files that keep changing.
const APPLY_TRIES: usize = 5;

/// Where a vault keeps its history: a hidden folder inside it, so that a
/// sandboxed app's access to the folder covers it, and named other than
/// `.git`, so editors and agents don't take the folder for a repository.
pub const HISTORY: &str = ".vaultsync";

/// A folder of notes with its history in [`HISTORY`] inside it.
#[derive(Clone)]
pub struct Vault {
    git_dir: PathBuf,
    folder: PathBuf,
}

fn git_dir_for(folder: &Path) -> PathBuf {
    folder.join(HISTORY)
}

/// Where the first releases kept a vault's history: beside it, `<folder>.git`.
fn beside(folder: &Path) -> PathBuf {
    let mut dir = folder.as_os_str().to_owned();
    dir.push(".git");
    dir.into()
}

/// Whether `tree` would write into a vault's history folder, in any case
/// (macOS folders don't tell `.VaultSync` from `.vaultsync`).
fn writes_into_history(tree: &Tree) -> bool {
    tree.iter()
        .any(|e| e.name_bytes().eq_ignore_ascii_case(HISTORY.as_bytes()))
}

/// Adds `line` to `file` unless it is there already.
fn ensure_line(file: &Path, line: &str) -> Result<()> {
    let existing = fs::read_to_string(file).unwrap_or_default();
    if existing.lines().any(|l| l == line) {
        return Ok(());
    }
    let mut text = format!("{line}\n");
    if !existing.is_empty() && !existing.ends_with('\n') {
        text.insert(0, '\n');
    }
    fs::create_dir_all(file.parent().expect("has a parent"))?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?
        .write_all(text.as_bytes())?;
    Ok(())
}

/// Where a fetched peer head is kept.
fn peer_ref(peer: &EndpointId) -> String {
    format!("refs/vaultsync/peers/{peer}")
}

/// The repository at `git_dir`, with `work_tree` as its files. Nothing in the
/// environment (`GIT_DIR` and the like) can point it elsewhere.
pub fn open_repo(git_dir: &Path, work_tree: &Path) -> Result<Repository> {
    let repo = Repository::open_ext(
        git_dir,
        RepositoryOpenFlags::NO_SEARCH | RepositoryOpenFlags::BARE,
        std::iter::empty::<&std::ffi::OsStr>(),
    )
    .with_context(|| format!("opening {}", git_dir.display()))?;
    repo.set_workdir(work_tree, false)?;
    Ok(repo)
}

/// An empty repository on branch `main`, its files kept elsewhere.
fn create_repo(git_dir: &Path) -> Result<Repository> {
    let repo = Repository::init_opts(
        git_dir,
        RepositoryInitOptions::new().bare(true).initial_head("main"),
    )?;
    repo.config()?.set_bool("core.bare", false)?;
    Ok(repo)
}

/// The commit `rev` names: an id, a short id, `HEAD~1`, a branch or a tag.
pub fn commit_id(repo: &Repository, rev: &str) -> Result<Oid> {
    Ok(repo
        .revparse_single(rev)
        .with_context(|| format!("no commit {rev}"))?
        .peel_to_commit()?
        .id())
}

/// An id as short as it can be while staying unambiguous (7 characters or more).
pub fn short(repo: &Repository, id: Oid) -> Result<String> {
    let short = repo.find_object(id, None)?.short_id()?;
    Ok(short.as_str().context("short id")?.to_owned())
}

/// The branch HEAD is on.
fn branch(repo: &Repository) -> Result<String> {
    let head = repo.find_reference("HEAD")?;
    Ok(head
        .symbolic_target()?
        .context("HEAD is not on a branch")?
        .to_owned())
}

/// Points HEAD's branch at `new`, only if it is still at `old` (or, for a
/// first commit, doesn't exist yet).
fn move_head(repo: &Repository, new: Oid, old: Option<Oid>) -> Result<Reference<'_>> {
    let name = branch(repo)?;
    Ok(match old {
        Some(old) => repo.reference_matching(&name, new, true, old, "vaultsync")?,
        None => repo.reference(&name, new, false, "vaultsync")?,
    })
}

/// The entry `path` names in `tree`, as (id, mode), if there is one.
fn entry_at(tree: &Tree, path: &Path) -> Option<(Oid, i32)> {
    tree.get_path(path).ok().map(|e| (e.id(), e.filemode()))
}

/// Whether `bytes` look binary the way git judges it: a NUL early on.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

impl Vault {
    fn at(folder: PathBuf) -> Vault {
        Vault {
            git_dir: git_dir_for(&folder),
            folder,
        }
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// The history folder.
    pub fn history(&self) -> &Path {
        &self.git_dir
    }

    pub fn repo(&self) -> Result<Repository> {
        open_repo(&self.git_dir, &self.folder)
    }

    /// This tool's own files inside the git directory.
    fn state(&self, name: &str) -> Result<PathBuf> {
        let dir = self.git_dir.join("vaultsync");
        fs::create_dir_all(&dir)?;
        Ok(dir.join(name))
    }

    /// Makes `folder` a vault. Existing history is adopted untouched: a `.git`
    /// inside the folder, or one beside it, is moved into [`HISTORY`]. A folder
    /// with no history gets a first commit of its contents.
    pub fn init(folder: &Path, machine: Option<String>) -> Result<Vault> {
        let created = !folder.exists();
        fs::create_dir_all(folder)?;
        let vault = Vault::at(folder.canonicalize()?);
        if let Err(e) = crate::config::refuse_overlap(vault.folder()) {
            if created {
                fs::remove_dir(vault.folder())?;
            }
            return Err(e);
        }
        let inner = vault.folder().join(".git");
        if fs::symlink_metadata(&inner).is_ok_and(|m| !m.is_dir()) {
            bail!(
                "{} is not a git directory; refusing to adopt it",
                inner.display()
            );
        }
        let found: Vec<PathBuf> = [inner, beside(vault.folder()), vault.git_dir.clone()]
            .into_iter()
            .filter(|d| d.is_dir())
            .collect();
        // Check existing history before moving anything.
        for git_dir in &found {
            branch(&open_repo(git_dir, vault.folder())?)
                .with_context(|| format!("HEAD in {} must be on a branch", git_dir.display()))?;
        }
        match found.as_slice() {
            [] => drop(create_repo(&vault.git_dir)?),
            [one] if *one == vault.git_dir => {}
            [one] => {
                fs::rename(one, &vault.git_dir).context("moving the history into the folder")?
            }
            many => bail!(
                "{} has more than one history ({}); refusing to choose between them",
                vault.folder().display(),
                many.iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
        let repo = vault.repo()?;
        let fresh = repo.head().is_err();
        // A bare snapshot has no index; build it from HEAD without touching refs.
        if !fresh && !vault.git_dir.join("index").exists() {
            let mut index = repo.index()?;
            index.read_tree(&repo.head()?.peel_to_tree()?)?;
            index.write()?;
        }
        let machine =
            machine.unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned());
        repo.config()?.set_str(MACHINE_KEY, &machine)?;
        vault.ensure_rules()?;
        if fresh {
            vault.stage_all(&repo)?;
            vault.commit(&repo, "Initialize vault")?;
        }
        Ok(vault)
    }

    /// Finds the vault containing `start`.
    pub fn open(start: &Path) -> Result<Vault> {
        let start = start.canonicalize()?;
        for dir in start.ancestors() {
            let vault = Vault::at(dir.to_owned());
            if !vault.git_dir.is_dir() && Vault::is_vault(&beside(dir), dir) {
                vault.move_history_inside()?;
            }
            if Vault::is_vault(&vault.git_dir, dir) {
                vault.ensure_rules()?;
                return Ok(vault);
            }
        }
        bail!("no vault at or above {}", start.display())
    }

    fn is_vault(git_dir: &Path, folder: &Path) -> bool {
        git_dir.is_dir()
            && open_repo(git_dir, folder)
                .and_then(|r| Ok(r.config()?.get_string(MACHINE_KEY)?))
                .is_ok()
    }

    /// Moves history kept beside the folder, as the first releases did, into
    /// it. The history itself is untouched: every ref and commit id stays.
    fn move_history_inside(&self) -> Result<()> {
        let old = beside(&self.folder);
        match fs::rename(&old, &self.git_dir) {
            Ok(()) => eprintln!(
                "moved the history from {} into {}",
                old.display(),
                self.git_dir.display()
            ),
            // Another vaultsync moved it first.
            Err(_) if self.git_dir.is_dir() => {}
            Err(e) => {
                return Err(e).with_context(|| format!("moving {} into the folder", old.display()));
            }
        }
        Ok(())
    }

    /// What every vault's history holds to: concurrent member additions merge
    /// as a union, never as markers, and the history is never one of the
    /// vault's files.
    fn ensure_rules(&self) -> Result<()> {
        let info = self.git_dir.join("info");
        ensure_line(
            &info.join("attributes"),
            &format!("{MEMBERS_PATH} merge=union"),
        )?;
        ensure_line(&info.join("exclude"), &format!("/{HISTORY}/"))
    }

    fn machine(&self) -> Result<String> {
        Ok(self.repo()?.config()?.get_string(MACHINE_KEY)?)
    }

    /// The full id of a commit.
    pub fn resolve(&self, rev: &str) -> Result<String> {
        Ok(commit_id(&self.repo()?, rev)?.to_string())
    }

    pub fn head(&self) -> Result<String> {
        Ok(self.head_id(&self.repo()?)?.to_string())
    }

    fn head_id(&self, repo: &Repository) -> Result<Oid> {
        repo.head()?.target().context("HEAD points nowhere")
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
        self.members_at("HEAD")
    }

    /// The machines trusted as of `rev`, plus this one.
    pub fn members_at(&self, rev: &str) -> Result<Members> {
        let mut members = members::at(&self.repo()?, rev)?.unwrap_or_default();
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
        let repo = self.repo()?;
        if fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            return short(&repo, self.head_id(&repo)?);
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
        let mut index = repo.index()?;
        index.add_path(Path::new(MEMBERS_PATH))?;
        index.write()?;
        self.commit(&repo, subject)
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
    /// folder that already has files, or history beside it from a first release.
    pub fn for_join(folder: &Path, machine: Option<String>) -> Result<(Vault, bool)> {
        // Resolve `.` and symlinks first, so any history beside the folder is
        // checked where it really is.
        let folder = std::path::absolute(folder)?;
        let (Some(parent), Some(name)) = (folder.parent(), folder.file_name()) else {
            bail!("{} can't hold a vault", folder.display());
        };
        fs::create_dir_all(parent)?;
        let folder = folder
            .canonicalize()
            .or_else(|_| parent.canonicalize().map(|p| p.join(name)))?;
        crate::config::refuse_overlap(&folder)?;
        let has_files = match fs::read_dir(&folder) {
            Ok(mut entries) => entries.next().is_some(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e).with_context(|| format!("can't read {}", folder.display())),
        };
        if has_files || beside(&folder).exists() {
            bail!(
                "{} already has files or history; join into an empty or new folder (--folder)",
                folder.display()
            );
        }
        let created = !folder.exists();
        fs::create_dir_all(&folder)?;
        let vault = Vault::at(folder);
        let repo = create_repo(&vault.git_dir)?;
        let machine =
            machine.unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned());
        repo.config()?.set_str(MACHINE_KEY, &machine)?;
        vault.ensure_rules()?;
        Ok((vault, created))
    }

    /// Takes the inviter's full history from its pack and fills the folder.
    pub fn adopt(&self, pack: &Path) -> Result<()> {
        let tip = self.import(pack)?;
        let repo = self.repo()?;
        if writes_into_history(&repo.find_commit(tip)?.tree()?) {
            bail!("the vault's history would write into its {HISTORY} folder");
        }
        // HEAD has no commit yet, so every file is new to the checkout, and
        // one someone saved there during the join stops it before it writes.
        repo.checkout_tree(
            repo.find_commit(tip)?.as_object(),
            Some(CheckoutBuilder::new().safe()),
        )
        .context("files appeared in the folder during the join")?;
        move_head(&repo, tip, None)?;
        Ok(())
    }

    /// Removes a vault [`for_join`](Self::for_join) made, after a failed join.
    pub fn discard(self, created_folder: bool) -> Result<()> {
        // Only what join wrote: files checked out and still as checked out,
        // their emptied folders, the history, and the folder if join
        // made it and nothing else landed there meanwhile.
        let repo = self.repo()?;
        let changed = self.changed_files(&repo)?;
        let index = repo.index()?;
        for entry in index.iter() {
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            if changed.contains(&path) {
                continue;
            }
            let file = self.folder().join(path);
            if fs::remove_file(&file).is_ok() {
                for dir in file.ancestors().skip(1).take_while(|d| *d != self.folder()) {
                    if fs::remove_dir(dir).is_err() {
                        break;
                    }
                }
            }
        }
        fs::remove_dir_all(&self.git_dir)?;
        if created_folder {
            let _ = fs::remove_dir(self.folder());
        }
        Ok(())
    }

    /// Tracked files whose contents on disk differ from the index.
    fn changed_files(&self, repo: &Repository) -> Result<Vec<String>> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(false).include_ignored(false);
        let changed = Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_TYPECHANGE;
        Ok(repo
            .statuses(Some(&mut opts))?
            .iter()
            .filter(|s| s.status().intersects(changed))
            .map(|s| String::from_utf8_lossy(s.path_bytes()).into_owned())
            .collect())
    }

    pub fn machine_name(&self) -> Result<String> {
        self.machine()
    }

    /// Serializes everything that writes to the index, the work tree or HEAD,
    /// and finishes a sync that a crash interrupted.
    fn lock(&self) -> Result<File> {
        let file = File::create(self.git_dir.join("vaultsync.lock"))?;
        file.lock()?;
        self.recover()?;
        Ok(file)
    }

    /// Commits the index as this machine, signed with its node key, plus the
    /// writing agent when `VAULTSYNC_AGENT` names one.
    fn commit(&self, repo: &Repository, subject: &str) -> Result<String> {
        let machine = self.machine()?;
        let tree = repo.find_tree(repo.index()?.write_tree()?)?;
        let parent = repo.head().ok().and_then(|h| h.target());
        let parents: Vec<String> = parent.iter().map(Oid::to_string).collect();
        let parents: Vec<&str> = parents.iter().map(String::as_str).collect();
        let mut message = format!("{subject}\n\n");
        if let Ok(agent) = std::env::var("VAULTSYNC_AGENT") {
            message.push_str(&format!("Agent: {agent}\n"));
        }
        message.push_str(&members::sign(
            &self.secret_key()?,
            &tree.id().to_string(),
            &parents,
        ));
        message.push('\n');
        let who = Signature::now(&machine, &format!("vaultsync@{machine}"))?;
        let parent_commits = parent.map(|p| repo.find_commit(p)).transpose()?;
        let id = repo.commit(
            None,
            &who,
            &who,
            &message,
            &tree,
            &parent_commits.iter().collect::<Vec<_>>(),
        )?;
        move_head(repo, id, parent)?;
        short(repo, id)
    }

    /// `git add -A`: the index takes every change in the folder, ignored
    /// files aside.
    fn stage_all(&self, repo: &Repository) -> Result<Index> {
        let mut index = repo.index()?;
        index.add_all(["*"], IndexAddOption::DEFAULT, None)?;
        index.update_all(["*"], None)?;
        index.write()?;
        Ok(index)
    }

    /// Commits every change in the folder, if there is any.
    pub fn commit_pending(&self) -> Result<Option<String>> {
        let _lock = self.lock()?;
        self.commit_pending_locked()
    }

    fn commit_pending_locked(&self) -> Result<Option<String>> {
        let repo = self.repo()?;
        let index = self.stage_all(&repo)?;
        let head = repo.head().ok().map(|h| h.peel_to_tree()).transpose()?;
        let diff = repo.diff_tree_to_index(head.as_ref(), Some(&index), None)?;
        let files: Vec<String> = diff
            .deltas()
            .filter_map(|d| d.new_file().path().or(d.old_file().path()))
            .map(|p| p.display().to_string())
            .collect();
        let subject = match files.as_slice() {
            [] => return Ok(None),
            [one] => format!("Update {one}"),
            many => format!("Update {} files", many.len()),
        };
        self.commit(&repo, &subject).map(Some)
    }

    /// How many paths have changes not yet committed.
    pub fn pending(&self) -> Result<usize> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true).include_ignored(false);
        Ok(self.repo()?.statuses(Some(&mut opts))?.len())
    }

    /// Files a merge left for a person: text with conflict markers, and the
    /// `*.conflict-<id>.*` copies kept beside binary files.
    pub fn conflicts(&self) -> Result<Vec<String>> {
        let repo = self.repo()?;
        let is_copy = |f: &str| {
            f.rsplit('/')
                .next()
                .unwrap_or(f)
                .split(".conflict-")
                .nth(1)
                .is_some_and(|rest| {
                    rest.as_bytes()
                        .get(..8)
                        .is_some_and(|id| id.iter().all(u8::is_ascii_hexdigit))
                })
        };
        // A merge's marker: seven `<`, a space, and a commit's 12-digit id.
        let marked = |line: &[u8]| {
            line.strip_prefix(b"<<<<<<< ")
                .and_then(|rest| rest.get(..12))
                .is_some_and(|id| id.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        };
        let mut files = vec![];
        for entry in repo.index()?.iter() {
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            let text = fs::read(self.folder().join(&path)).unwrap_or_default();
            if is_copy(&path) || (!is_binary(&text) && text.split(|b| *b == b'\n').any(marked)) {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }

    /// Where the running daemon keeps what it knows about its peers.
    pub fn daemon_state(&self) -> Result<PathBuf> {
        self.state("daemon.json")
    }

    pub fn status(&self) -> Result<String> {
        let count = self.pending()?;
        let repo = self.repo()?;
        let head = repo.head()?.peel_to_commit()?;
        Ok(format!(
            "vault:   {}\nhistory: {}\nmachine: {}\nnode:    {}\nbranch:  {}\nhead:    {} {}\n{count} uncommitted change{}\n",
            self.folder().display(),
            self.git_dir.display(),
            self.machine()?,
            self.node_id()?,
            branch(&repo)?.trim_start_matches("refs/heads/"),
            short(&repo, head.id())?,
            String::from_utf8_lossy(head.summary_bytes().unwrap_or_default()),
            if count == 1 { "" } else { "s" },
        ))
    }

    /// Commits newest first, each as `<id>  <date>  <machine>  <subject>`.
    /// With `path`, only those that changed it.
    pub fn log(&self, limit: usize, path: Option<&Path>) -> Result<String> {
        let repo = self.repo()?;
        let mut walk = repo.revwalk()?;
        walk.push_head()?;
        let mut out = String::new();
        let mut shown = 0;
        for id in walk {
            if shown == limit {
                break;
            }
            let commit = repo.find_commit(id?)?;
            if let Some(path) = path {
                let here = entry_at(&commit.tree()?, path);
                let mut parents = commit.parents();
                let changed = match parents.len() {
                    0 => here.is_some(),
                    _ => parents.all(|p| p.tree().is_ok_and(|t| entry_at(&t, path) != here)),
                };
                if !changed {
                    continue;
                }
            }
            let when = commit.time();
            out.push_str(&format!(
                "{}  {}  {}  {}\n",
                short(&repo, commit.id())?,
                datetime(when.seconds() + i64::from(when.offset_minutes()) * 60),
                String::from_utf8_lossy(commit.author().name_bytes()),
                String::from_utf8_lossy(commit.summary_bytes().unwrap_or_default()),
            ));
            shown += 1;
        }
        Ok(out)
    }

    /// Restores the whole vault, or some paths, to their state at `rev` as a
    /// new commit. Pending edits are committed first, so nothing is lost.
    pub fn rollback(&self, rev: &str, paths: &[PathBuf]) -> Result<String> {
        let _lock = self.lock()?;
        // Resolve first: the checkpoint below moves HEAD, and with it `HEAD~1`.
        let (id, short_id) = {
            let repo = self.repo()?;
            let id = commit_id(&repo, rev)?;
            (id, short(&repo, id)?)
        };
        self.commit_pending_locked()?;
        let repo = self.repo()?;
        let head = repo.head()?.peel_to_commit()?;
        let then = repo.find_commit(id)?.tree()?;
        let now = head.tree()?;
        let (mut tree, what) = if paths.is_empty() {
            (then.id(), "vault".to_owned())
        } else {
            let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
            // `a/./b` is `a/b`, and `.` the whole vault.
            let paths: Vec<PathBuf> = paths
                .iter()
                .map(|p| p.components().filter(|c| *c != Component::CurDir).collect())
                .collect();
            if let Some(p) = paths.iter().find(|p| {
                !p.as_os_str().is_empty()
                    && entry_at(&then, p).is_none()
                    && entry_at(&now, p).is_none()
            }) {
                bail!("{} is in neither {short_id} nor the vault now", p.display());
            }
            (tree_with(&repo, &now, &then, &paths)?, names.join(", "))
        };
        // A rollback never undoes a membership change, whatever it selects:
        // the member list stays as it is now.
        let members = Path::new(MEMBERS_PATH);
        let target = repo.find_tree(tree)?;
        if entry_at(&target, members) != entry_at(&now, members) {
            tree = tree_with(&repo, &target, &now, &[members.to_owned()])?;
        }
        if tree == now.id() {
            bail!("{what} already matches {short_id}");
        }
        // Refuses, before writing anything, to overwrite a file saved since
        // the checkpoint or an ignored one in the way.
        repo.checkout_tree(
            repo.find_tree(tree)?.as_object(),
            Some(CheckoutBuilder::new().safe().overwrite_ignored(false)),
        )
        .context("rollback would overwrite files changed since, or not tracked; move them aside")?;
        self.commit(&repo, &format!("Roll back {what} to {short_id}"))
    }

    /// Commit ids to tell a peer this machine already has.
    pub fn haves(&self) -> Result<Vec<String>> {
        let repo = self.repo()?;
        let mut haves = vec![];
        for r in repo.references_glob("refs/vaultsync/peers/*")? {
            haves.extend(r?.target().map(|id| id.to_string()));
        }
        haves.push(self.head_id(&repo)?.to_string());
        Ok(haves)
    }

    /// The subset of `ids` that are commits here.
    pub fn present(&self, ids: &[String]) -> Result<Vec<String>> {
        let repo = self.repo()?;
        Ok(ids
            .iter()
            .filter(|id| Oid::from_str(id).is_ok_and(|oid| repo.find_commit(oid).is_ok()))
            .cloned()
            .collect())
    }

    /// Whether `head` has commits that `haves` lack.
    pub fn has_news(&self, head: &str, haves: &[String]) -> Result<bool> {
        let repo = self.repo()?;
        Ok(walk_new(&repo, head, haves)?.next().is_some())
    }

    /// `head` and a pack of everything it holds that `haves` lack: the id on
    /// its own line, then the pack.
    pub fn pack(&self, head: &str, haves: &[String]) -> Result<Vec<u8>> {
        let repo = self.repo()?;
        let mut walk = walk_new(&repo, head, haves)?;
        let mut builder = repo.packbuilder()?;
        builder.insert_walk(&mut walk)?;
        let mut pack = git2::Buf::new();
        builder.write_buf(&mut pack)?;
        let mut out = format!("{head}\n").into_bytes();
        out.extend_from_slice(&pack);
        Ok(out)
    }

    /// Path for a pack arriving from `peer`.
    pub fn incoming(&self, peer: &EndpointId) -> Result<PathBuf> {
        // A fresh name per attempt: no two transfers ever share a file.
        static ATTEMPT: AtomicU64 = AtomicU64::new(0);
        let attempt = ATTEMPT.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        self.state(&format!(
            "incoming-{}-{pid}-{attempt}.pack",
            peer.fmt_short()
        ))
    }

    /// Stores the objects of a [`pack`](Self::pack) and returns its head,
    /// removing the file either way.
    fn import(&self, path: &Path) -> Result<Oid> {
        let imported = fs::read(path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| {
                let split = bytes
                    .iter()
                    .position(|b| *b == b'\n')
                    .context("malformed pack")?;
                let head = Oid::from_str(std::str::from_utf8(&bytes[..split])?)?;
                let repo = self.repo()?;
                let odb = repo.odb()?;
                let mut writer = odb.packwriter()?;
                writer.write_all(&bytes[split + 1..])?;
                writer.commit()?;
                repo.find_commit(head)
                    .context("the pack lacks its own head")?;
                Ok(head)
            });
        fs::remove_file(path)?;
        imported
    }

    /// Stores a peer's head from its pack.
    pub fn receive(&self, peer: &EndpointId, pack: &Path) -> Result<()> {
        let tip = self.import(pack)?;
        self.repo()?
            .reference(&peer_ref(peer), tip, true, "fetched")?;
        Ok(())
    }

    /// Brings a peer's head into this vault: fast-forward, or a merge. Pending
    /// edits are committed first, and a file edited while the sync runs is
    /// never overwritten. Returns the new HEAD if it moved.
    pub fn integrate(&self, peer: &EndpointId) -> Result<Option<String>> {
        let _lock = self.lock()?;
        let Ok(tip) = self.repo()?.refname_to_id(&peer_ref(peer)) else {
            return Ok(None);
        };
        for _ in 0..APPLY_TRIES {
            self.commit_pending_locked()?;
            let repo = self.repo()?;
            // History straight from a peer removed meanwhile counts for nothing;
            // what a current member relays is judged commit by commit.
            if !self.members()?.contains(peer) {
                bail!(
                    "refusing history from {}: no longer a member",
                    peer.fmt_short()
                );
            }
            let head = self.head_id(&repo)?;
            if tip == head || repo.graph_descendant_of(head, tip)? {
                return Ok(None);
            }
            members::validate(&repo, head, tip, &self.members()?)
                .with_context(|| format!("refusing history from {}", peer.fmt_short()))?;
            // Merge only when each side has edits the other lacks. A head that
            // already holds all of ours is taken as is; merging it again is how
            // a ring of peers would keep re-merging the same edits forever.
            let ours_covered = edits_beyond(&repo, head, tip)? == 0;
            let theirs_covered = edits_beyond(&repo, tip, head)? == 0;
            let new = match (ours_covered, theirs_covered) {
                // Same edits, different merges: the greater id wins everywhere.
                (true, true) if head > tip => return Ok(None),
                (true, _) => tip,
                // We hold all of theirs and more; they take ours.
                (false, true) => return Ok(None),
                (false, false) => merge(&repo, head, tip)?,
            };
            if self.apply(&repo, head, new)? {
                return Ok(Some(new.to_string()));
            }
        }
        bail!("files kept changing; will retry")
    }

    /// Moves HEAD, index and work tree from `old` to `new`. False if a file
    /// that would change was edited meanwhile; nothing is touched then. The
    /// intent file records how far it got, for `recover`.
    fn apply(&self, repo: &Repository, old: Oid, new: Oid) -> Result<bool> {
        if hooked("pause-before-apply") {
            eprintln!("test hook: pausing before apply");
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        // A tracked file deleted since the checkpoint isn't in the checkout's
        // way and would be written back, so a rename made meanwhile would
        // leave both names. Any tracked change since then: commit it and
        // merge again.
        if !self.changed_files(repo)?.is_empty() {
            return Ok(false);
        }
        if writes_into_history(&repo.find_commit(new)?.tree()?) {
            bail!("commit {new} would write into the vault's {HISTORY} folder");
        }
        let intent = self.state("apply")?;
        write_whole(&intent, &format!("started {old} {new}\n"))?;
        crash_if_hooked("crash-before-worktree");
        let checkout = repo.checkout_tree(
            repo.find_commit(new)?.as_object(),
            Some(CheckoutBuilder::new().safe()),
        );
        match checkout {
            Err(e) if e.code() == ErrorCode::Conflict => {
                fs::remove_file(&intent)?;
                return Ok(false);
            }
            checkout => checkout?,
        }
        write_whole(&intent, &format!("written {old} {new}\n"))?;
        crash_if_hooked("crash-after-worktree");
        move_head(repo, new, Some(old))?;
        fs::remove_file(&intent)?;
        Ok(true)
    }

    /// Settles an `apply` a crash interrupted, never undoing what a user did
    /// since. Once the work tree was fully written only HEAD is left to move,
    /// and any difference from `new` is the user's. Before that, part of `new`
    /// may be on disk: HEAD stays at `old`, and whatever is on disk is
    /// committed as an edit and merged again.
    fn recover(&self) -> Result<()> {
        let intent = self.state("apply")?;
        let Ok(text) = fs::read_to_string(&intent) else {
            return Ok(());
        };
        let [phase, old, new] = text.split_whitespace().collect::<Vec<_>>()[..] else {
            bail!("corrupt apply intent: {text}");
        };
        let repo = self.repo()?;
        let (old, new) = (Oid::from_str(old)?, Oid::from_str(new)?);
        if phase == "written" && self.head_id(&repo)? == old {
            move_head(&repo, new, Some(old))?;
        }
        fs::remove_file(&intent)?;
        Ok(())
    }
}

/// Commits reachable from `head` but not from any of `haves`.
fn walk_new<'r>(repo: &'r Repository, head: &str, haves: &[String]) -> Result<git2::Revwalk<'r>> {
    let mut walk = repo.revwalk()?;
    walk.push(Oid::from_str(head)?)?;
    for have in haves {
        walk.hide(Oid::from_str(have)?)?;
    }
    Ok(walk)
}

/// Edits (non-merge commits) reachable from `a` but not from `b`.
fn edits_beyond(repo: &Repository, a: Oid, b: Oid) -> Result<usize> {
    let mut walk = repo.revwalk()?;
    walk.push(a)?;
    walk.hide(b)?;
    let mut edits = 0;
    for id in walk {
        if repo.find_commit(id?)?.parent_count() <= 1 {
            edits += 1;
        }
    }
    Ok(edits)
}

/// `base` with `paths` as they are in `source` (absent where it lacks them).
fn tree_with(repo: &Repository, base: &Tree, source: &Tree, paths: &[PathBuf]) -> Result<Oid> {
    let mut index = Index::new()?;
    index.read_tree(base)?;
    for path in paths {
        if path.as_os_str().is_empty() {
            index.clear()?;
            index.read_tree(source)?;
            continue;
        }
        index.remove_path(path)?;
        index.remove_dir(path, 0)?;
        let Ok(entry) = source.get_path(path) else {
            continue;
        };
        let prefix = path.to_string_lossy().into_owned();
        if entry.kind() != Some(ObjectType::Tree) {
            index.add(&index_entry(
                prefix.as_bytes(),
                entry.filemode(),
                entry.id(),
            ))?;
            continue;
        }
        let mut files = vec![];
        repo.find_tree(entry.id())?
            .walk(TreeWalkMode::PreOrder, |dir, e| {
                if e.kind() != Some(ObjectType::Tree) {
                    let name = format!("{prefix}/{dir}{}", String::from_utf8_lossy(e.name_bytes()));
                    files.push((name, e.filemode(), e.id()));
                }
                TreeWalkResult::Ok
            })?;
        for (name, mode, id) in files {
            index.add(&index_entry(name.as_bytes(), mode, id))?;
        }
    }
    Ok(index.write_tree_to(repo)?)
}

/// `YYYY-MM-DD HH:MM:SS` for seconds since the epoch, already shifted to the
/// wanted time zone.
fn datetime(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Replaces `path` by renaming a finished copy over it: a crash leaves the
/// old contents or the new, never an empty or half-written file.
fn write_whole(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(tmp, path)?;
    Ok(())
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

#[cfg(test)]
mod tests {
    #[test]
    fn dates_are_civil() {
        assert_eq!(super::datetime(0), "1970-01-01 00:00:00");
        assert_eq!(super::datetime(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(super::datetime(1_763_300_000), "2025-11-16 13:33:20");
    }
}
