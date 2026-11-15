use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

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
        vault
            .git
            .run(["symbolic-ref", "-q", "HEAD"])
            .context("HEAD must be on a branch")?;
        let machine = match machine {
            Some(m) => m,
            None => gethostname::gethostname().to_string_lossy().into_owned(),
        };
        vault.git.run(["config", MACHINE_KEY, &machine])?;
        // Concurrent member additions merge as a union, never as markers.
        let attributes = beside.join("info").join("attributes");
        let rule = format!("{MEMBERS_PATH} merge=union\n");
        if !fs::read_to_string(&attributes)
            .unwrap_or_default()
            .contains(&rule)
        {
            fs::create_dir_all(attributes.parent().expect("has a parent"))?;
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&attributes)?
                .write_all(rule.as_bytes())?;
        }
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
                return Ok(vault);
            }
        }
        bail!("no vault at or above {}", start.display())
    }

    fn machine(&self) -> Result<String> {
        self.git.value(["config", MACHINE_KEY])
    }

    fn head(&self) -> Result<String> {
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
        members.insert(self.node_id()?, self.machine()?);
        Ok(members)
    }

    /// The other members, which this machine syncs with.
    pub fn peers(&self) -> Result<Vec<EndpointId>> {
        let me = self.node_id()?;
        Ok(self.members()?.into_keys().filter(|id| *id != me).collect())
    }

    /// Adds a machine to the member list, in a commit signed by this one.
    pub fn add_member(&self, id: EndpointId, name: &str) -> Result<String> {
        let _lock = self.lock()?;
        self.commit_pending_locked()?;
        let mut members = self.members()?;
        members.insert(id, name.to_owned());
        let path = self.folder().join(MEMBERS_PATH);
        fs::create_dir_all(path.parent().expect("has a parent"))?;
        fs::write(&path, members::format(&members))?;
        self.git.run(["add", MEMBERS_PATH])?;
        self.commit(&format!("Add member {name}"))
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
        self.commit_pending_locked()?;
        let id = self
            .git
            .value(["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?;
        let short = self.git.value(["rev-parse", "--short", &id])?;
        let target = match path {
            None => {
                self.git.run(["read-tree", "-u", "--reset", &id])?;
                "vault".to_owned()
            }
            Some(path) => {
                let mut cmd = self.git.command(["restore", "--staged", "--worktree"]);
                cmd.arg(format!("--source={id}")).arg("--").arg(path);
                exec(cmd)?;
                path.display().to_string()
            }
        };
        if self
            .git
            .run(["diff", "--cached", "--name-only"])?
            .is_empty()
        {
            bail!("{target} already matches {short}");
        }
        self.commit(&format!("Roll back {target} to {short}"))
    }

    /// Commit ids to tell a peer this machine already has.
    pub fn haves(&self) -> Result<Vec<String>> {
        let refs = self.git.run([
            "for-each-ref",
            "--format=%(objectname)",
            "refs/vaultsync/peers",
        ])?;
        Ok(refs.lines().map(str::to_owned).chain(self.head()).collect())
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
        let mut cmd = self
            .git
            .command(["bundle", "create", "-q", "-", "HEAD", "--not"]);
        cmd.args(haves);
        cmd
    }

    /// Path for a bundle arriving from `peer`.
    pub fn incoming(&self, peer: &EndpointId) -> Result<PathBuf> {
        self.state(&format!("incoming-{}.bundle", peer.fmt_short()))
    }

    /// Stores a peer's head from its bundle. True if it moved.
    pub fn receive(&self, peer: &EndpointId, bundle: &Path) -> Result<bool> {
        let name = peer_ref(peer);
        let before = self.git.value(["rev-parse", "-q", "--verify", &name]).ok();
        let mut cmd = self.git.command(["fetch", "-q", "--no-write-fetch-head"]);
        cmd.arg(bundle).arg(format!("+HEAD:{name}"));
        exec(cmd)?;
        fs::remove_file(bundle)?;
        Ok(self.git.value(["rev-parse", &name]).ok() != before)
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
            let new = if self.is_ancestor(&head, &tip) {
                tip.clone()
            } else {
                merge(&self.git, &head, &tip)?
            };
            if self.apply(&head, &new)? {
                return Ok(Some(new));
            }
        }
        bail!("files kept changing; will retry")
    }

    fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.git.run(["merge-base", "--is-ancestor", a, b]).is_ok()
    }

    /// Moves HEAD, index and work tree from `old` to `new`. False if a file
    /// git would change was edited meanwhile; nothing is touched then.
    fn apply(&self, old: &str, new: &str) -> Result<bool> {
        let intent = self.state("apply")?;
        fs::write(&intent, format!("{old} {new}\n"))?;
        if self.git.run(["read-tree", "-m", "-u", old, new]).is_err() {
            fs::remove_file(&intent)?;
            return Ok(false);
        }
        if std::env::var("VAULTSYNC_TEST_HOOK").as_deref() == Ok("crash-mid-apply") {
            std::process::exit(9);
        }
        self.git.run(["update-ref", "HEAD", new, old])?;
        fs::remove_file(&intent)?;
        Ok(true)
    }

    /// Finishes an `apply` a crash interrupted: files still at `old` move to
    /// `new`; files that differ from both are edits, kept for the next commit.
    fn recover(&self) -> Result<()> {
        let intent = self.state("apply")?;
        let Ok(text) = fs::read_to_string(&intent) else {
            return Ok(());
        };
        let (old, new) = text
            .trim()
            .split_once(' ')
            .context("corrupt apply intent")?;
        if self.head()? == old {
            self.git.run(["read-tree", new])?;
            let changed = self.git.run(["diff", "--name-only", "-z", old, new])?;
            for path in changed.split('\0').filter(|p| !p.is_empty()) {
                let file = self.folder().join(path);
                let was = self
                    .git
                    .value(["rev-parse", "-q", "--verify", &format!("{old}:{path}")])
                    .ok();
                let is = file
                    .exists()
                    .then(|| self.git.value(["hash-object", "--", path]).ok())
                    .flatten();
                if is != was {
                    continue;
                }
                if self
                    .git
                    .run(["cat-file", "-e", &format!("{new}:{path}")])
                    .is_ok()
                {
                    self.git.run(["checkout-index", "-f", "--", path])?;
                } else if file.exists() {
                    fs::remove_file(&file)?;
                }
            }
            self.git.run(["update-ref", "HEAD", new, old])?;
        }
        fs::remove_file(&intent)?;
        Ok(())
    }
}
