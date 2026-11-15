use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const MACHINE: &str = "test-machine";

fn vaultsync(vault: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vaultsync"))
        .arg("--vault")
        .arg(vault)
        .args(args)
        .env_remove("VAULTSYNC_AGENT")
        .output()
        .unwrap()
}

fn ok(vault: &Path, args: &[&str]) -> String {
    let out = vaultsync(vault, args);
    assert!(
        out.status.success(),
        "vaultsync {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn init(folder: &Path) -> String {
    ok(
        folder.parent().unwrap(),
        &["init", folder.to_str().unwrap(), "--machine", MACHINE],
    )
}

/// Plain git, independent of the code under test.
fn git(dir: &Path, args: &[&str]) -> String {
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

fn git_dir(folder: &Path) -> PathBuf {
    PathBuf::from(format!("{}.git", folder.display()))
}

/// Every ref with its commit id, plus HEAD.
fn refs(git_dir: &Path) -> String {
    let gd = format!("--git-dir={}", git_dir.display());
    git(
        Path::new("."),
        &[&gd, "for-each-ref", "--format=%(objectname) %(refname)"],
    ) + &git(Path::new("."), &[&gd, "rev-parse", "HEAD"])
}

/// A repo with some history: two commits on main, a tag, and a side branch.
fn existing_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    fs::write(dir.join("a.md"), "one\n").unwrap();
    git(dir, &["add", "a.md"]);
    git(dir, &["commit", "-qm", "first"]);
    git(dir, &["tag", "v1"]);
    git(dir, &["branch", "side"]);
    fs::write(dir.join("a.md"), "one\ntwo\n").unwrap();
    git(dir, &["commit", "-qam", "second"]);
}

#[test]
fn init_fresh_folder_keeps_history_beside_it() {
    // A folder with no git, and one that was `git init`ed but never committed.
    for unborn in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        fs::create_dir_all(folder.join("sub")).unwrap();
        fs::write(folder.join("a.md"), "hello\n").unwrap();
        if unborn {
            git(&folder, &["init", "-q"]);
        }

        init(&folder);

        assert!(
            !folder.join(".git").exists(),
            "vault folder must stay clean"
        );
        assert!(git_dir(&folder).is_dir());
        // Found from a subfolder, without --vault pointing at the root.
        let log = ok(&folder.join("sub"), &["log"]);
        assert!(
            log.contains(MACHINE) && log.contains("Initialize vault"),
            "{log}"
        );
        let gd = format!("--git-dir={}", git_dir(&folder).display());
        assert_eq!(git(tmp.path(), &[&gd, "ls-files"]), "a.md\n");

        fs::write(folder.join("b.md"), "new\n").unwrap();
        let status = ok(&folder, &["status"]);
        assert!(status.contains("1 uncommitted change"), "{status}");
    }
}

#[test]
fn init_adopts_existing_history_unchanged() {
    // An ordinary clone (.git inside the folder) and a bare snapshot beside it.
    for layout in ["inner", "beside"] {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        existing_repo(&src);
        let folder = tmp.path().join("notes");
        if layout == "inner" {
            git(tmp.path(), &["clone", "-q", "src", "notes"]);
            git(&folder, &["branch", "side", "origin/side"]);
        } else {
            git(tmp.path(), &["clone", "-q", "--bare", "src", "notes.git"]);
            fs::create_dir(&folder).unwrap();
            fs::copy(src.join("a.md"), folder.join("a.md")).unwrap();
        }
        let before = refs(&if layout == "inner" {
            folder.join(".git")
        } else {
            git_dir(&folder)
        });

        init(&folder);

        assert!(!folder.join(".git").exists(), "{layout}");
        assert_eq!(refs(&git_dir(&folder)), before, "{layout}: refs changed");
        let log = ok(&folder, &["log"]);
        assert!(
            log.contains("second") && log.contains("first"),
            "{layout}: {log}"
        );
        let status = ok(&folder, &["status"]);
        assert!(
            status.contains("0 uncommitted changes"),
            "{layout}: {status}"
        );
    }
}

#[test]
fn init_refuses_folder_with_history_in_two_places() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("notes");
    existing_repo(&folder);
    fs::create_dir(git_dir(&folder)).unwrap();

    let out = vaultsync(tmp.path(), &["init", folder.to_str().unwrap()]);

    assert!(!out.status.success());
    assert!(folder.join(".git").is_dir(), "nothing may be moved");
}

#[test]
fn edit_becomes_commit_within_seconds() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("notes");
    init(&folder);
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_vaultsync"))
        .arg("--vault")
        .arg(&folder)
        .arg("run")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(daemon.stderr.take().unwrap());
    let mut line = String::new();
    stderr.read_line(&mut line).unwrap();
    assert!(line.contains("watching"), "{line}");

    let start = Instant::now();
    fs::create_dir(folder.join("people")).unwrap();
    fs::write(folder.join("people/ada.md"), "# Ada\n").unwrap();
    let log = loop {
        let log = ok(&folder, &["log", "-n", "1"]);
        if log.contains("people/ada.md") || start.elapsed() > Duration::from_secs(10) {
            break log;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    daemon.kill().unwrap();
    daemon.wait().unwrap();

    println!("edit committed after {:?}: {log}", start.elapsed());
    assert!(log.contains("Update people/ada.md"), "no commit: {log}");
    assert!(log.contains(MACHINE), "{log}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn rollback_restores_prior_version_as_new_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("notes");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("a.md"), "v1\n").unwrap();
    init(&folder);
    let first = ok(&folder, &["log"])
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    fs::write(folder.join("a.md"), "v2\n").unwrap();
    fs::write(folder.join("b.md"), "b\n").unwrap();

    // One file: the pending edit is committed first, then a.md goes back to v1.
    let out = ok(&folder, &["rollback", &first, "a.md"]);
    assert_eq!(fs::read_to_string(folder.join("a.md")).unwrap(), "v1\n");
    assert!(folder.join("b.md").exists());
    let log = ok(&folder, &["log"]);
    assert_eq!(log.lines().count(), 3, "{log}");
    assert!(
        log.lines()
            .next()
            .unwrap()
            .contains(&format!("Roll back a.md to {first}")),
        "{out}{log}"
    );
    assert!(log.contains("Update 2 files"), "{log}");

    // The whole vault: b.md, added later, goes away.
    ok(&folder, &["rollback", &first]);
    assert!(!folder.join("b.md").exists());
    let log = ok(&folder, &["log"]);
    assert_eq!(log.lines().count(), 4, "{log}");
    assert!(
        log.lines()
            .next()
            .unwrap()
            .contains(&format!("Roll back vault to {first}")),
        "{log}"
    );
}
