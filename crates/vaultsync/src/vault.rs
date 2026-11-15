use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
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
        let fresh = match (fs::symlink_metadata(&inner).ok(), beside.exists()) {
            (Some(meta), false) if meta.is_dir() => {
                fs::rename(&inner, beside).context("moving .git beside the folder")?;
                false
            }
            (Some(_), _) => bail!(
                "{} has its own .git and {} also exists; refusing to choose between them",
                vault.folder().display(),
                beside.display()
            ),
            (None, true) => {
                // A bare snapshot has no index; build it from HEAD without touching refs.
                if !beside.join("index").exists() {
                    vault.git.run(["read-tree", "HEAD"])?;
                }
                false
            }
            (None, false) => {
                vault.git.run(["init", "-q", "-b", "main"])?;
                true
            }
        };
        vault
            .git
            .run(["symbolic-ref", "-q", "HEAD"])
            .context("HEAD must be on a branch")?;
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
        let mut cmd = self
            .git
            .command(["commit", "-q", "--no-verify", "-m", &message]);
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
        self.commit(&format!("Roll back {target} to {short}"), false)
    }

    /// Watches the folder and commits each burst of edits once it settles.
    pub fn run(&self) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        let mut debouncer = new_debouncer(DEBOUNCE, tx)?;
        debouncer
            .watcher()
            .watch(self.folder(), RecursiveMode::Recursive)?;
        eprintln!("watching {}", self.folder().display());
        // Catch edits made while nothing was watching.
        self.report(self.commit_pending());
        for events in rx {
            match events {
                Ok(_) => self.report(self.commit_pending()),
                Err(e) => eprintln!("watch error: {e}"),
            }
        }
        Ok(())
    }

    fn report(&self, result: Result<Option<String>>) {
        match result {
            Ok(Some(id)) => eprintln!("committed {id}"),
            Ok(None) => {}
            Err(e) => eprintln!("commit failed, will retry on the next edit: {e:#}"),
        }
    }
}
