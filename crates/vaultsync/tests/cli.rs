use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod common;
use common::{git, ok, vaultsync};
use std::time::{Duration, Instant};

const MACHINE: &str = "test-machine";

fn init(folder: &Path) -> String {
    ok(
        folder.parent().unwrap(),
        &["init", folder.to_str().unwrap(), "--machine", MACHINE],
    )
}

fn start_daemon(folder: &Path, extra: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_vaultsync"))
        .arg("--vault")
        .arg(folder)
        .args(["run", "--addr-dir"])
        .arg(folder.parent().unwrap())
        .args(extra)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
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
fn init_refuses_unusable_history_and_moves_nothing() {
    for case in ["history in two places", "detached HEAD"] {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        existing_repo(&folder);
        if case == "history in two places" {
            fs::create_dir(git_dir(&folder)).unwrap();
        } else {
            git(&folder, &["checkout", "-q", "--detach"]);
        }

        let out = vaultsync(tmp.path(), &["init", folder.to_str().unwrap()]);

        assert!(!out.status.success(), "{case}");
        assert!(folder.join(".git").is_dir(), "{case}: nothing may be moved");
        if case == "detached HEAD" {
            assert!(!git_dir(&folder).exists(), "{case}");
        }
    }
}

#[test]
fn failed_auto_commit_is_retried_without_another_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("notes");
    init(&folder);
    let mut daemon = start_daemon(&folder, &[]);
    let mut stderr = BufReader::new(daemon.stderr.take().unwrap());

    // Another git process holds the index, so the commit fails.
    let index_lock = git_dir(&folder).join("index.lock");
    fs::write(&index_lock, "").unwrap();
    fs::write(folder.join("held.md"), "saved while git was busy\n").unwrap();
    let mut line = String::new();
    while !line.contains("commit failed") {
        line.clear();
        assert_ne!(stderr.read_line(&mut line).unwrap(), 0, "daemon exited");
    }
    fs::remove_file(&index_lock).unwrap();

    let start = Instant::now();
    while !ok(&folder, &["log", "-n", "1"]).contains("Update held.md") {
        assert!(start.elapsed() < Duration::from_secs(10), "never retried");
        std::thread::sleep(Duration::from_millis(100));
    }
    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

#[test]
fn rollback_never_overwrites_a_file_it_does_not_track() {
    // (tracked once, ignored local file that now sits in its place)
    for (old, local) in [("draft.tmp", "draft.tmp"), ("drafts", "drafts/scratch.tmp")] {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join(old), "old tracked draft\n").unwrap();
        init(&folder);
        let first = ok(&folder, &["log"])
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        // Later it is untracked, drafts are ignored, and someone keeps a local one.
        let (gd, wt) = (
            format!("--git-dir={}", git_dir(&folder).display()),
            format!("--work-tree={}", folder.display()),
        );
        fs::write(folder.join(".gitignore"), "*.tmp\n").unwrap();
        git(&folder, &[&gd, &wt, "rm", "-q", old]);
        git(&folder, &[&gd, &wt, "add", ".gitignore"]);
        git(&folder, &[&gd, &wt, "commit", "-qm", "ignore drafts"]);
        fs::create_dir_all(folder.join(local).parent().unwrap()).unwrap();
        fs::write(folder.join(local), "local scratch\n").unwrap();

        let out = vaultsync(&folder, &["rollback", &first]);

        assert!(!out.status.success(), "{local}: rollback must refuse");
        assert_eq!(
            fs::read_to_string(folder.join(local)).unwrap(),
            "local scratch\n"
        );
        assert!(
            folder.join(".gitignore").exists(),
            "{local}: nothing restored halfway"
        );
    }
}

#[test]
fn edit_becomes_commit_within_seconds() {
    // With the OS's file notifications, and by polling.
    for extra in [&[][..], &["--poll"]] {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        init(&folder);
        let mut daemon = start_daemon(&folder, extra);
        let mut stderr = BufReader::new(daemon.stderr.take().unwrap());
        let mut line = String::new();
        stderr.read_line(&mut line).unwrap();
        assert!(line.contains("watching"), "{extra:?}: {line}");

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

        println!(
            "{extra:?}: edit committed after {:?}: {log}",
            start.elapsed()
        );
        assert!(
            log.contains("Update people/ada.md"),
            "{extra:?}: no commit: {log}"
        );
        assert!(log.contains(MACHINE), "{log}");
        assert!(start.elapsed() < Duration::from_secs(5), "{extra:?}");
    }
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
    let out = ok(&folder, &["rollback", "HEAD", "a.md"]);
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
