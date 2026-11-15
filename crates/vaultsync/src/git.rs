use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The system `git`, pointed at a work tree whose git directory lives elsewhere.
pub struct Git {
    pub git_dir: PathBuf,
    pub work_tree: PathBuf,
}

impl Git {
    pub fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(["-c", "commit.gpgsign=false", "-c", "core.quotepath=false"])
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(&self.work_tree);
        // Inherited from a parent git process (a hook, say), these would point git elsewhere.
        for (var, _) in std::env::vars_os() {
            if var.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(var);
            }
        }
        // Paths are file names, never globs: `[draft].md` matches only itself.
        cmd.env("GIT_LITERAL_PATHSPECS", "1");
        cmd
    }

    /// Runs git and returns stdout, failing with git's stderr.
    pub fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        exec(self.command(args))
    }

    /// Like `run`, with the output's trailing newline trimmed.
    pub fn value<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(self.run(args)?.trim_end().to_owned())
    }
}

pub fn exec(mut cmd: Command) -> Result<String> {
    let out = cmd.output().context("running git")?;
    if !out.status.success() {
        bail!(
            "{:?} failed: {}",
            cmd.get_args().collect::<Vec<_>>(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
