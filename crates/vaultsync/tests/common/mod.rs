#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// The binary under test, with its own config so `init` and `join` never
/// add to the real list of vaults.
pub fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vaultsync"));
    cmd.env(
        "XDG_CONFIG_HOME",
        std::env::temp_dir().join("vaultsync-tests-config"),
    );
    cmd
}

pub fn vaultsync(vault: &Path, args: &[&str]) -> Output {
    bin()
        .arg("--vault")
        .arg(vault)
        .args(args)
        .env_remove("VAULTSYNC_AGENT")
        .output()
        .unwrap()
}

pub fn ok(vault: &Path, args: &[&str]) -> String {
    let out = vaultsync(vault, args);
    assert!(
        out.status.success(),
        "vaultsync {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Plain git, independent of the code under test.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=someone",
            "-c",
            "user.email=someone@example.com",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// After a restart, peers reach the daemon again once their connections to
/// the dead instance idle out: about 15 s at worst (35 s with QUIC's default).
pub const AFTER_RESTART: u64 = 25;

/// Polls `cond` until it holds or `secs` pass; true if it held.
pub fn wait_until(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    cond()
}

/// One machine: a vault folder and, while running, its daemon.
pub struct Node {
    pub folder: PathBuf,
    pub name: String,
    pub id: String,
    pub log: PathBuf,
    pub daemon: Option<Child>,
}

impl Node {
    pub fn new(root: &Path, name: &str) -> Node {
        let folder = root.join(name);
        ok(root, &["init", folder.to_str().unwrap(), "--machine", name]);
        let id = ok(&folder, &["id"]).trim().to_owned();
        Node {
            log: root.join(format!("{name}.log")),
            folder,
            name: name.into(),
            id,
            daemon: None,
        }
    }

    /// A machine that joins with `code`; Err carries what `join` printed.
    pub fn join(root: &Path, name: &str, code: &str, addrs: &Path) -> Result<Node, String> {
        let folder = root.join(name);
        let out = vaultsync(
            root,
            &[
                "join",
                code,
                "--folder",
                folder.to_str().unwrap(),
                "--machine",
                name,
                "--addr-dir",
                addrs.to_str().unwrap(),
            ],
        );
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        let id = ok(&folder, &["id"]).trim().to_owned();
        Ok(Node {
            log: root.join(format!("{name}.log")),
            folder,
            name: name.into(),
            id,
            daemon: None,
        })
    }

    /// A fresh invite code from this machine.
    pub fn invite(&self, expires_in: u64) -> String {
        let out = ok(
            &self.folder,
            &["invite", "--expires-in", &expires_in.to_string()],
        );
        let line = out
            .lines()
            .find_map(|l| l.strip_prefix("Or on the new machine: vaultsync join "));
        line.expect("invite prints a join command")
            .trim()
            .to_owned()
    }

    pub fn trust(&self, other: &Node) {
        ok(
            &self.folder,
            &["member", "add", &other.id, "--name", &other.name],
        );
    }

    pub fn start_with(&mut self, addrs: &Path, hook: Option<&str>) {
        let mut cmd = bin();
        cmd.arg("--vault")
            .arg(&self.folder)
            .args(["run", "--catch-up", "1", "--addr-dir"])
            .arg(addrs);
        if let Some(hook) = hook {
            cmd.env("VAULTSYNC_TEST_HOOK", hook);
        }
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        // The log spans restarts: wait for this start's line, not an old one.
        let started = format!("as node {}", self.id);
        let before = self.log_text().matches(&started).count();
        self.daemon = Some(cmd.stderr(log).stdout(Stdio::null()).spawn().unwrap());
        assert!(
            wait_until(10, || self.log_text().matches(&started).count() > before),
            "{} never started:\n{}",
            self.name,
            self.log_text()
        );
    }

    pub fn stop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            d.kill().unwrap();
            d.wait().unwrap();
        }
    }

    /// Waits until the daemon has committed every edit.
    pub fn committed(&self) {
        let clean = || ok(&self.folder, &["status"]).contains("0 uncommitted changes");
        assert!(
            wait_until(10, clean),
            "{} kept uncommitted edits:\n{}",
            self.name,
            self.log_text()
        );
    }

    pub fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub fn head(&self) -> String {
        git(
            Path::new("."),
            &[
                &format!("--git-dir={}.git", self.folder.display()),
                "rev-parse",
                "HEAD",
            ],
        )
    }

    pub fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.folder.join(path)).ok()
    }

    pub fn write(&self, path: &str, text: &str) {
        let file = self.folder.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `n` machines that each added every other one by hand.
pub struct Mesh {
    pub _tmp: tempfile::TempDir,
    pub addrs: PathBuf,
    pub nodes: Vec<Node>,
}

impl Mesh {
    pub fn new(n: usize) -> Mesh {
        let tmp = tempfile::tempdir().unwrap();
        let addrs = tmp.path().join("addrs");
        fs::create_dir(&addrs).unwrap();
        let nodes: Vec<Node> = (0..n)
            .map(|i| Node::new(tmp.path(), &format!("m{i}")))
            .collect();
        for a in &nodes {
            for b in nodes.iter().filter(|b| b.id != a.id) {
                a.trust(b);
            }
        }
        Mesh {
            _tmp: tmp,
            addrs,
            nodes,
        }
    }

    pub fn start(&mut self, i: usize) {
        self.nodes[i].start_with(&self.addrs.clone(), None);
    }

    pub fn start_all(&mut self) {
        (0..self.nodes.len()).for_each(|i| self.start(i));
    }

    pub fn stop_all(&mut self) {
        self.nodes.iter_mut().for_each(Node::stop);
    }

    /// Waits for every machine to have the same head, and returns it.
    pub fn converged(&self, secs: u64) -> String {
        let same = || self.nodes.iter().all(|n| n.head() == self.nodes[0].head());
        assert!(wait_until(secs, same), "no convergence:\n{}", self.logs());
        self.nodes[0].head()
    }

    pub fn logs(&self) -> String {
        self.nodes
            .iter()
            .map(|n| format!("--- {}\n{}", n.name, n.log_text()))
            .collect()
    }
}
