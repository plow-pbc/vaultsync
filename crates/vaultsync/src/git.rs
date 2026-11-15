use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

/// The system `git`, pointed at a work tree whose git directory lives elsewhere.
#[derive(Clone)]
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
            .args(args)
            .current_dir(&self.work_tree);
        // Inherited from a parent git process (a hook, say), these would point git elsewhere.
        for (var, _) in std::env::vars_os() {
            if var.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(var);
            }
        }
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

    /// Runs git with `input` on stdin.
    pub fn run_stdin<I, S>(&self, args: I, input: &str) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.command(args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("running git")?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(input.as_bytes())?;
        check(&cmd, child.wait_with_output()?)
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
    check(&cmd, out)
}

fn check(cmd: &Command, out: Output) -> Result<String> {
    if !out.status.success() {
        bail!(
            "{:?} failed: {}",
            cmd.get_args().collect::<Vec<_>>(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
