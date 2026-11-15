use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode};

use crate::git::{Git, exec};

const MACHINE_KEY: &str = "vaultsync.machine";
const DEBOUNCE: Duration = Duration::from_millis(500);

/// A folder of notes whose git directory sits beside it (`<folder>.git`), so
/// editors and agents never see `.git`.
pub struct Vault {
    git: Git,
}

fn git_dir_for(folder: &Path) -> PathBuf {
    let mut dir = folder.as_os_str().to_owned();
    dir.push(".git");
    dir.into()
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
        let fresh = vault
            .git
            .run(["rev-parse", "--verify", "-q", "HEAD"])
            .is_err();
        // A bare snapshot has no index; build it from HEAD without touching refs.
        if !fresh && !beside.join("index").exists() {
            vault.git.run(["read-tree", "HEAD"])?;
        }
        let machine = match machine {
            Some(m) => m,
            None => gethostname::gethostname().to_string_lossy().into_owned(),
        };
        vault.git.run(["config", MACHINE_KEY, &machine])?;
        if fresh {
            vault.git.run(["add", "-A"])?;
            vault.commit("Initialize vault", true)?;
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

    /// Serializes everything that writes to the index or makes commits.
    fn lock(&self) -> Result<File> {
        let file = File::create(self.git.git_dir.join("vaultsync.lock"))?;
        file.lock()?;
        Ok(file)
    }

    /// Commits the staged changes as this machine, plus the writing agent when
    /// `VAULTSYNC_AGENT` names one.
    fn commit(&self, subject: &str, allow_empty: bool) -> Result<String> {
        let machine = self.machine()?;
        let email = format!("vaultsync@{machine}");
        let mut message = subject.to_owned();
        if let Ok(agent) = std::env::var("VAULTSYNC_AGENT") {
            message.push_str(&format!("\n\nAgent: {agent}"));
        }
        let mut cmd = self.git.command(["commit", "-q", "-m", &message]);
        if allow_empty {
            cmd.arg("--allow-empty");
        }
        for (key, value) in [("NAME", &machine), ("EMAIL", &email)] {
            cmd.env(format!("GIT_AUTHOR_{key}"), value)
                .env(format!("GIT_COMMITTER_{key}"), value);
        }
        exec(cmd)?;
        self.git.value(["rev-parse", "--short", "HEAD"])
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
        self.commit(&subject, false).map(Some)
    }

    pub fn status(&self) -> Result<String> {
        let pending = self.git.run(["status", "--porcelain", "-z"])?;
        let count = pending.split('\0').filter(|e| !e.is_empty()).count();
        Ok(format!(
            "vault:   {}\nhistory: {}\nmachine: {}\nbranch:  {}\nhead:    {}\n{count} uncommitted change{}\n",
            self.folder().display(),
            self.git.git_dir.display(),
            self.machine()?,
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
        let (tree, what) = match path {
            None => (
                self.git.value(["rev-parse", &format!("{id}^{{tree}}")])?,
                "vault".to_owned(),
            ),
            Some(path) => (self.tree_with(&id, path)?, path.display().to_string()),
        };
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
        self.commit(&format!("Roll back {what} to {short}"), false)
    }

    /// HEAD's tree with `path` as it was at `id` (absent if it was absent),
    /// built in a scratch index so the real one is untouched.
    fn tree_with(&self, id: &str, path: &Path) -> Result<String> {
        let index = self.git.git_dir.join("vaultsync-rollback-index");
        let with_index = |args: &[&OsStr]| {
            let mut cmd = self.git.command(args);
            cmd.env("GIT_INDEX_FILE", &index);
            exec(cmd)
        };
        let source = format!("--source={id}");
        with_index(&["read-tree".as_ref(), "HEAD".as_ref()])?;
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

    /// Watches the folder and commits each burst of edits once it settles.
    /// A failed commit is retried with backoff, edits or not.
    pub fn run(&self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        let mut debouncer = new_debouncer(DEBOUNCE, tx)?;
        debouncer
            .watcher()
            .watch(self.folder(), RecursiveMode::Recursive)?;
        // Catch edits made while nothing was watching.
        let mut retry = self.attempt(None);
        eprintln!("watching {}", self.folder().display());
        loop {
            let event = match retry {
                Some(wait) => rx.recv_timeout(wait),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match event {
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
                Ok(Err(e)) => eprintln!("watch error: {e}"),
                Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => retry = self.attempt(retry),
            }
        }
    }

    /// Commits pending edits; on failure returns how long to wait before
    /// trying again, doubling `retry` up to a minute.
    fn attempt(&self, retry: Option<Duration>) -> Option<Duration> {
        match self.commit_pending() {
            Ok(id) => {
                if let Some(id) = id {
                    eprintln!("committed {id}");
                }
                None
            }
            Err(e) => {
                let wait = retry.map_or(Duration::from_secs(1), |w| {
                    (w * 2).min(Duration::from_secs(60))
                });
                eprintln!("commit failed, retrying in {}s: {e:#}", wait.as_secs());
                Some(wait)
            }
        }
    }
}
