use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod common;
use common::{git, ok, wait_until};

/// One machine: a vault folder and, while running, its daemon.
struct Node {
    folder: PathBuf,
    name: String,
    id: String,
    log: PathBuf,
    daemon: Option<Child>,
}

impl Node {
    fn new(root: &Path, name: &str) -> Node {
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

    fn trust(&self, other: &Node) {
        ok(
            &self.folder,
            &["member", "add", &other.id, "--name", &other.name],
        );
    }

    fn start_with(&mut self, addrs: &Path, hook: Option<&str>) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vaultsync"));
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
        self.daemon = Some(cmd.stderr(log).stdout(Stdio::null()).spawn().unwrap());
        assert!(
            wait_until(10, || self
                .log_text()
                .contains(&format!("as node {}", self.id))),
            "{} never started:\n{}",
            self.name,
            self.log_text()
        );
    }

    fn stop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            d.kill().unwrap();
            d.wait().unwrap();
        }
    }

    fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn head(&self) -> String {
        git(
            Path::new("."),
            &[
                &format!("--git-dir={}.git", self.folder.display()),
                "rev-parse",
                "HEAD",
            ],
        )
    }

    fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.folder.join(path)).ok()
    }

    fn write(&self, path: &str, text: &str) {
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
struct Mesh {
    _tmp: tempfile::TempDir,
    addrs: PathBuf,
    nodes: Vec<Node>,
}

impl Mesh {
    fn new(n: usize) -> Mesh {
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

    fn start(&mut self, i: usize) {
        self.nodes[i].start_with(&self.addrs.clone(), None);
    }

    fn start_all(&mut self) {
        (0..self.nodes.len()).for_each(|i| self.start(i));
    }

    fn stop_all(&mut self) {
        self.nodes.iter_mut().for_each(Node::stop);
    }

    /// Waits for every machine to have the same head, and returns it.
    fn converged(&self, secs: u64) -> String {
        let same = || self.nodes.iter().all(|n| n.head() == self.nodes[0].head());
        assert!(wait_until(secs, same), "no convergence:\n{}", self.logs());
        self.nodes[0].head()
    }

    fn logs(&self) -> String {
        self.nodes
            .iter()
            .map(|n| format!("--- {}\n{}", n.name, n.log_text()))
            .collect()
    }
}

#[test]
fn edits_reach_every_member_and_heads_settle() {
    let mut mesh = Mesh::new(3);
    mesh.start_all();

    mesh.nodes[0].write("people/ada.md", "# Ada\n");
    mesh.nodes[2].write("people/grace.md", "# Grace\n");
    let all_have = |path: &str, text: &str| {
        mesh.nodes
            .iter()
            .all(|n| n.read(path).as_deref() == Some(text))
    };
    assert!(
        wait_until(20, || all_have("people/ada.md", "# Ada\n")
            && all_have("people/grace.md", "# Grace\n")),
        "{}",
        mesh.logs()
    );

    // Idle: one identical head everywhere, and catch-up cycles add nothing.
    let head = mesh.converged(20);
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        mesh.nodes.iter().all(|n| n.head() == head),
        "heads moved while idle:\n{}",
        mesh.logs()
    );
    // The history that arrived is the history that was made: m0's first commit is everywhere.
    let first = git(
        &mesh.nodes[0].folder,
        &[
            &format!("--git-dir={}.git", mesh.nodes[0].folder.display()),
            "rev-list",
            "--max-parents=0",
            "HEAD",
        ],
    );
    for n in &mesh.nodes {
        let gd = format!("--git-dir={}.git", n.folder.display());
        assert_eq!(
            git(&n.folder, &[&gd, "rev-list", "--max-parents=0", "HEAD"]),
            first
        );
    }
}

#[test]
fn non_member_is_refused() {
    let mut mesh = Mesh::new(1);
    let mut outsider = Node::new(mesh._tmp.path(), "outsider");
    outsider.trust(&mesh.nodes[0]);
    outsider.write("spam.md", "let me in\n");
    mesh.start(0);
    outsider.start_with(&mesh.addrs, None);

    let refused = format!("refused connection from {}", &outsider.id[..10]);
    assert!(
        wait_until(10, || mesh.nodes[0].log_text().contains(&refused)),
        "{}",
        mesh.logs()
    );
    assert!(
        wait_until(10, || outsider.log_text().contains("not a member")),
        "{}",
        outsider.log_text()
    );
    assert_eq!(mesh.nodes[0].read("spam.md"), None);
}

#[test]
fn concurrent_offline_edits_merge_or_keep_both() {
    let mut mesh = Mesh::new(3);
    let lines: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    mesh.nodes[0].write("notes.md", &lines);
    mesh.nodes[0].write("same.md", "one\n");
    mesh.nodes[0].write("gone.md", "keep me\n");
    mesh.nodes[0].write("old-name.md", "a\nb\nc\nd\n");
    mesh.start_all();
    mesh.converged(20);
    mesh.stop_all();

    // Two machines edit while apart.
    let (a, b) = (&mesh.nodes[0], &mesh.nodes[1]);
    a.write("notes.md", &lines.replace("line 1\n", "line ONE\n"));
    b.write("notes.md", &lines.replace("line 10\n", "line TEN\n"));
    a.write("same.md", "zero\n");
    b.write("same.md", "uno\n");
    fs::remove_file(a.folder.join("gone.md")).unwrap();
    b.write("gone.md", "keep me, edited\n");
    fs::rename(a.folder.join("old-name.md"), a.folder.join("new-name.md")).unwrap();
    b.write("old-name.md", "a\nb\nc\nD\n");
    fs::write(a.folder.join("pic.png"), b"\x89PNG\0from a").unwrap();
    fs::write(b.folder.join("pic.png"), b"\x89PNG\0from b").unwrap();
    let (extra_a, extra_b) = (
        Node::new(mesh._tmp.path(), "extra-a"),
        Node::new(mesh._tmp.path(), "extra-b"),
    );
    a.trust(&extra_a);
    b.trust(&extra_b);

    mesh.start_all();
    mesh.converged(30);
    for n in &mesh.nodes {
        let status = ok(&n.folder, &["status"]);
        assert!(
            status.contains("0 uncommitted changes"),
            "{}: {status}",
            n.name
        );
    }
    let n = &mesh.nodes[2];
    assert_eq!(
        n.read("notes.md").unwrap(),
        lines
            .replace("line 1\n", "line ONE\n")
            .replace("line 10\n", "line TEN\n")
    );
    let same = n.read("same.md").unwrap();
    assert!(
        same.contains("<<<<<<<") && same.contains("zero") && same.contains("uno"),
        "{same}"
    );
    assert_eq!(n.read("gone.md").as_deref(), Some("keep me, edited\n"));
    assert_eq!(n.read("new-name.md").as_deref(), Some("a\nb\nc\nD\n"));
    assert_eq!(n.read("old-name.md"), None);
    let mut pics: Vec<Vec<u8>> = fs::read_dir(&n.folder)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("pic"))
        .map(|p| fs::read(p).unwrap())
        .collect();
    pics.sort();
    assert_eq!(
        pics,
        vec![b"\x89PNG\0from a".to_vec(), b"\x89PNG\0from b".to_vec()]
    );
    let members = n.read(".vault/members").unwrap();
    assert!(
        members.contains(&extra_a.id)
            && members.contains(&extra_b.id)
            && !members.contains("<<<<<<<"),
        "{members}"
    );

    // Fixing the markers is an ordinary edit, and it syncs too.
    n.write("same.md", "zero and uno\n");
    assert!(
        wait_until(20, || mesh
            .nodes
            .iter()
            .all(|m| m.read("same.md").as_deref() == Some("zero and uno\n"))),
        "{}",
        mesh.logs()
    );
    mesh.converged(20);
}

#[test]
fn edit_made_during_an_interrupted_sync_survives() {
    let mut mesh = Mesh::new(2);
    mesh.nodes[1].write("from-b.md", "b\n");
    mesh.start(1);
    // m0 dies after writing m1's files but before moving HEAD.
    let addrs = mesh.addrs.clone();
    mesh.nodes[0].start_with(&addrs, Some("crash-mid-apply"));
    let a = &mut mesh.nodes[0];
    assert!(
        wait_until(20, || a
            .daemon
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_some()),
        "never crashed:\n{}",
        a.log_text()
    );
    a.daemon = None;
    assert_eq!(a.read("from-b.md").as_deref(), Some("b\n"));

    a.write("during.md", "written while the sync was interrupted\n");
    mesh.start(0);
    assert!(
        wait_until(20, || mesh.nodes[1].read("during.md").is_some()),
        "{}",
        mesh.logs()
    );
    mesh.converged(20);
    let log = ok(&mesh.nodes[0].folder, &["log"]);
    assert!(
        log.contains("Update during.md") && log.contains("Merge"),
        "{log}"
    );
    assert_eq!(mesh.nodes[0].read("from-b.md").as_deref(), Some("b\n"));
}

#[test]
fn unsigned_member_change_is_refused() {
    let mut mesh = Mesh::new(2);
    let rogue = Node::new(mesh._tmp.path(), "rogue");
    // m1's history gains a member change made without vaultsync's signature.
    let b = &mesh.nodes[1];
    let mut members = b.read(".vault/members").unwrap();
    members.push_str(&format!("{} rogue\n", rogue.id));
    b.write(".vault/members", &members);
    b.write("smuggled.md", "x\n");
    let gd = format!("--git-dir={}.git", b.folder.display());
    let wt = format!("--work-tree={}", b.folder.display());
    git(&b.folder, &[&gd, &wt, "add", "-A"]);
    git(&b.folder, &[&gd, &wt, "commit", "-qm", "sneak a member in"]);

    mesh.start_all();
    let refused = format!("refusing history from {}", &mesh.nodes[1].id[..10]);
    assert!(
        wait_until(15, || mesh.nodes[0].log_text().contains(&refused)),
        "{}",
        mesh.logs()
    );
    assert_eq!(mesh.nodes[0].read("smuggled.md"), None);
    assert!(
        !mesh.nodes[0]
            .read(".vault/members")
            .unwrap()
            .contains(&rogue.id)
    );
}
