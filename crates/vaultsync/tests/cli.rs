use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;

mod common;
use common::{bin, git, history_dir, ok, vaultsync};
use std::time::{Duration, Instant};

const MACHINE: &str = "test-machine";

fn init(folder: &Path) -> String {
    ok(
        folder.parent().unwrap(),
        &["init", folder.to_str().unwrap(), "--machine", MACHINE],
    )
}

fn start_daemon(folder: &Path, extra: &[&str], hook: &str) -> std::process::Child {
    bin()
        .env("VAULTSYNC_TEST_HOOK", hook)
        .arg("--vault")
        .arg(folder)
        .args(["run", "--addr-dir"])
        .arg(folder.parent().unwrap())
        .args(extra)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
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
fn init_fresh_folder_keeps_history_inside_it() {
    // A folder with no git, and one that was `git init`ed but never committed.
    for unborn in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        fs::create_dir_all(folder.join("sub")).unwrap();
        fs::write(folder.join("a.md"), "hello\n").unwrap();
        // An ignore rule that would bring the history, and its key, back in.
        fs::write(
            folder.join(".gitignore"),
            "!*\n!.vaultsync/\n!.vaultsync/**\n",
        )
        .unwrap();
        if unborn {
            git(&folder, &["init", "-q"]);
        }

        init(&folder);

        assert!(
            !folder.join(".git").exists(),
            "vault folder must stay clean"
        );
        assert!(history_dir(&folder).is_dir());
        // Found from a subfolder, without --vault pointing at the root.
        let log = ok(&folder.join("sub"), &["log"]);
        assert!(
            log.contains(MACHINE) && log.contains("Initialize vault"),
            "{log}"
        );
        // The history is never one of the vault's files.
        let gd = format!("--git-dir={}", history_dir(&folder).display());
        assert_eq!(git(tmp.path(), &[&gd, "ls-files"]), ".gitignore\na.md\n");

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
            tmp.path().join("notes.git")
        });

        init(&folder);

        assert!(!folder.join(".git").exists(), "{layout}");
        assert!(!tmp.path().join("notes.git").exists(), "{layout}");
        assert_eq!(
            refs(&history_dir(&folder)),
            before,
            "{layout}: refs changed"
        );
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
            fs::create_dir(tmp.path().join("notes.git")).unwrap();
        } else {
            git(&folder, &["checkout", "-q", "--detach"]);
        }

        let out = vaultsync(tmp.path(), &["init", folder.to_str().unwrap()]);

        assert!(!out.status.success(), "{case}");
        assert!(folder.join(".git").is_dir(), "{case}: nothing may be moved");
        if case == "detached HEAD" {
            assert!(!history_dir(&folder).exists(), "{case}");
        }
    }
}

#[test]
fn a_held_index_is_waited_for_and_one_left_by_a_killed_process_is_cleared() {
    for left_by_a_kill in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        init(&folder);
        let index_lock = history_dir(&folder).join("index.lock");
        let lock = fs::File::create(&index_lock).unwrap();
        if left_by_a_kill {
            // Written minutes ago by a process that never got to remove it.
            let then = std::time::SystemTime::now() - Duration::from_secs(120);
            lock.set_modified(then).unwrap();
        }
        let mut daemon = start_daemon(&folder, &[], "");
        let mut stderr = BufReader::new(daemon.stderr.take().unwrap());
        fs::write(folder.join("held.md"), "saved while git was busy\n").unwrap();
        let mut line = String::new();
        let awaited = if left_by_a_kill {
            "removed"
        } else {
            "commit failed"
        };
        let waiting = Instant::now();
        while !line.contains(awaited) {
            assert!(
                waiting.elapsed() < Duration::from_secs(20),
                "never logged {awaited:?}"
            );
            line.clear();
            assert_ne!(stderr.read_line(&mut line).unwrap(), 0, "daemon exited");
        }
        // Another git process holding it lets go; a dead one never would.
        if !left_by_a_kill {
            fs::remove_file(&index_lock).unwrap();
        }

        let start = Instant::now();
        while !ok(&folder, &["log", "-n", "1"]).contains("Update held.md") {
            assert!(start.elapsed() < Duration::from_secs(10), "never committed");
            std::thread::sleep(Duration::from_millis(100));
        }
        daemon.kill().unwrap();
        daemon.wait().unwrap();
    }
}

#[test]
fn a_second_daemon_on_a_vault_waits_for_the_first_to_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("notes");
    init(&folder);
    let mut first = start_daemon(&folder, &[], "");
    let mut first_err = BufReader::new(first.stderr.take().unwrap());
    let mut line = String::new();
    while !line.contains("watching") {
        line.clear();
        assert_ne!(
            first_err.read_line(&mut line).unwrap(),
            0,
            "first daemon exited"
        );
    }
    let mut second = start_daemon(&folder, &[], "");
    let mut second_err = BufReader::new(second.stderr.take().unwrap());
    line.clear();
    second_err.read_line(&mut line).unwrap();
    assert!(
        line.contains("another vaultsync is running this vault"),
        "{line}"
    );

    // Once the first stops, the second takes over.
    first.kill().unwrap();
    first.wait().unwrap();
    line.clear();
    while !line.contains("watching") {
        line.clear();
        assert_ne!(
            second_err.read_line(&mut line).unwrap(),
            0,
            "second daemon exited"
        );
    }
    fs::write(folder.join("after.md"), "saved under the second daemon\n").unwrap();
    let start = Instant::now();
    while !ok(&folder, &["log", "-n", "1"]).contains("Update after.md") {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "second daemon never committed"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    second.kill().unwrap();
    second.wait().unwrap();
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
            format!("--git-dir={}", history_dir(&folder).display()),
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
    // With the OS's file notifications, by polling, and by polling because
    // notifications failed to start.
    for (extra, hook) in [(&[][..], ""), (&["--poll"], ""), (&[], "no-native-watch")] {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        init(&folder);
        let mut daemon = start_daemon(&folder, extra, hook);
        let mut stderr = BufReader::new(daemon.stderr.take().unwrap());
        let mut line = String::new();
        if !hook.is_empty() {
            stderr.read_line(&mut line).unwrap();
            assert!(line.contains("checking for changes every 2 s"), "{line}");
            line.clear();
        }
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
fn a_vault_inside_another_stays_out_of_its_history() {
    let tmp = tempfile::tempdir().unwrap();
    let outer = tmp.path().join("wiki");
    // A folder the outer vault tracks becomes a vault, and another moves in.
    fs::create_dir_all(outer.join("old")).unwrap();
    fs::write(outer.join("old/note.md"), "before\n").unwrap();
    init(&outer);
    init(&outer.join("old"));
    let moved = tmp.path().join("company");
    init(&moved);
    fs::write(moved.join("runbook.md"), "company\n").unwrap();
    fs::rename(&moved, outer.join("plowco")).unwrap();
    fs::write(outer.join("old/note.md"), "after\n").unwrap();
    fs::write(outer.join("mine.md"), "mine\n").unwrap();

    let mut daemon = start_daemon(&outer, &[], "");
    let start = Instant::now();
    while !ok(&outer, &["log", "-n", "1"]).contains("mine.md")
        && start.elapsed() < Duration::from_secs(10)
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    daemon.kill().unwrap();
    daemon.wait().unwrap();

    let gd = format!("--git-dir={}", history_dir(&outer).display());
    let files = git(
        Path::new("."),
        &[&gd, "ls-tree", "-r", "--name-only", "HEAD"],
    );
    assert_eq!(
        files, "mine.md\nold/note.md\n",
        "an inner vault was committed"
    );
    let note = git(Path::new("."), &[&gd, "show", "HEAD:old/note.md"]);
    assert_eq!(note, "before\n", "an inner vault's edit was committed");
    let status = ok(&outer, &["status"]);
    assert!(status.contains("0 uncommitted"), "{status}");
}

#[test]
fn rollback_of_several_paths_is_one_commit_or_none() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("notes");
    fs::create_dir_all(folder.join("people")).unwrap();
    fs::write(folder.join("people/ada.md"), "v1\n").unwrap();
    fs::write(folder.join("hot.md"), "v1\n").unwrap();
    fs::write(folder.join("other.md"), "v1\n").unwrap();
    init(&folder);
    let first = ok(&folder, &["log"])
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    for f in ["people/ada.md", "hot.md", "other.md"] {
        fs::write(folder.join(f), "v2\n").unwrap();
    }
    fs::write(folder.join("people/new.md"), "made since\n").unwrap();
    let read = |f: &str| fs::read_to_string(folder.join(f)).ok();
    let commits = || ok(&folder, &["log"]).lines().count();

    // A path that exists at neither commit: nothing is rolled back.
    let out = vaultsync(&folder, &["rollback", &first, "people", "nowhere"]);
    assert!(!out.status.success());
    assert_eq!(read("people/ada.md").as_deref(), Some("v2\n"));
    assert!(read("people/new.md").is_some());
    let before = commits();

    // Both paths in one commit; what's outside them stays.
    ok(&folder, &["rollback", &first, "people", "hot.md"]);
    assert_eq!(read("people/ada.md").as_deref(), Some("v1\n"));
    assert_eq!(read("people/new.md"), None);
    assert_eq!(read("hot.md").as_deref(), Some("v1\n"));
    assert_eq!(read("other.md").as_deref(), Some("v2\n"));
    assert_eq!(commits(), before + 1);
    let log = ok(&folder, &["log"]);
    assert!(
        log.lines()
            .next()
            .unwrap()
            .contains(&format!("Roll back people, hot.md to {first}")),
        "{log}"
    );
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

#[test]
fn one_service_runs_every_tracked_vault_including_ones_added_later() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config");
    let run = |args: &[&str]| run_with_config(&config, args);
    let folder = |name: &str| {
        tmp.path()
            .join(name)
            .canonicalize()
            .unwrap_or(tmp.path().join(name))
    };
    run(&["init", folder("a").to_str().unwrap(), "--machine", MACHINE]);
    run(&["init", folder("b").to_str().unwrap(), "--machine", MACHINE]);
    run(&["--vault", folder("a").to_str().unwrap(), "track"]);
    assert_eq!(fs::read_dir(config.join("vaults")).unwrap().count(), 2);
    // A vault may be neither inside the settings nor around them.
    // Spelled elsewhere, landing there: `..` after a symlinked folder.
    fs::create_dir(tmp.path().join("sibling")).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("sibling"), tmp.path().join("alias")).unwrap();
    let around = tmp.path().join("alias/../config/notes");
    for overlapping in [config.join("notes"), tmp.path().to_owned(), around] {
        let out = bin()
            .env("VAULTSYNC_CONFIG_DIR", &config)
            .args(["init", overlapping.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{}", overlapping.display());
    }
    assert!(!config.join("notes").exists());

    let mut service = bin()
        .env("VAULTSYNC_CONFIG_DIR", &config)
        .args(["run", "--all", "--catch-up", "1", "--addr-dir"])
        .arg(tmp.path())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let committed = |name: &str, file: &str| {
        common::wait_until(15, || {
            run(&["--vault", folder(name).to_str().unwrap(), "log"])
                .contains(&format!("Update {file}"))
        })
    };
    fs::write(folder("a").join("note.md"), "in a\n").unwrap();
    fs::write(folder("b").join("note.md"), "in b\n").unwrap();
    assert!(committed("a", "note.md") && committed("b", "note.md"));

    // A vault made while the service runs is picked up.
    run(&["init", folder("c").to_str().unwrap(), "--machine", MACHINE]);
    std::thread::sleep(Duration::from_secs(6));
    fs::write(folder("c").join("note.md"), "in c\n").unwrap();
    assert!(committed("c", "note.md"));

    // A listed vault that has since gone reports an error; the rest still show.
    run(&[
        "init",
        folder("gone").to_str().unwrap(),
        "--machine",
        MACHINE,
    ]);
    fs::remove_dir_all(folder("gone")).unwrap();
    let all: serde_json::Value =
        serde_json::from_str(&run(&["status", "--json", "--all"])).unwrap();
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 4);
    for vault in &all[..3] {
        assert_eq!(vault["running"], true, "{vault}");
        assert_eq!(vault["pending"], 0, "{vault}");
        assert_eq!(vault["machine"], MACHINE);
    }
    assert!(all[3]["error"].is_string(), "{}", all[3]);
    service.kill().unwrap();
    service.wait().unwrap();
}

#[test]
fn a_vault_from_a_first_release_moves_its_history_inside_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("notes");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("a.md"), "one\n").unwrap();
    init(&folder);
    fs::write(folder.join("a.md"), "one\ntwo\n").unwrap();
    // The first releases kept the history beside the folder.
    let beside = tmp.path().join("notes.git");
    fs::rename(history_dir(&folder), &beside).unwrap();
    let before = refs(&beside);

    let status = ok(&folder, &["status"]);

    assert!(!beside.exists());
    assert_eq!(refs(&history_dir(&folder)), before, "refs changed");
    assert!(status.contains("1 uncommitted change"), "{status}");
    let log = ok(&folder, &["log"]);
    assert!(log.contains("Initialize vault"), "{log}");
}

/// Linux only: macOS refuses file names that aren't UTF-8.
#[cfg(target_os = "linux")]
#[test]
fn rollback_of_a_folder_keeps_names_that_are_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("notes");
    let names = [b"\xfe.md".as_slice(), b"\xff.md".as_slice()];
    let file = |name: &[u8]| folder.join("raw").join(std::ffi::OsStr::from_bytes(name));
    fs::create_dir_all(folder.join("raw")).unwrap();
    for name in names {
        fs::write(file(name), "v1\n").unwrap();
    }
    init(&folder);
    let first = ok(&folder, &["log"])
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    fs::remove_dir_all(folder.join("raw")).unwrap();

    ok(&folder, &["rollback", &first, "raw"]);

    for name in names {
        assert_eq!(fs::read_to_string(file(name)).unwrap(), "v1\n");
    }
}

#[test]
fn a_new_vault_takes_the_name_this_machine_already_uses() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config");
    let run = |args: &[&str]| run_with_config(&config, args);
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    run(&["init", a.to_str().unwrap(), "--machine", "air"]);
    assert!(run(&["init", b.to_str().unwrap()]).contains("machine: air"));

    // No engine has run it yet: not stopped, just not started.
    let status: serde_json::Value =
        serde_json::from_str(&run(&["--vault", b.to_str().unwrap(), "status", "--json"])).unwrap();
    assert_eq!(status["started"], false, "{status}");
    assert_eq!(status["running"], false, "{status}");
}

/// Runs vaultsync with its own settings directory; its output.
fn run_with_config(config: &Path, args: &[&str]) -> String {
    let out = bin()
        .env("VAULTSYNC_CONFIG_DIR", config)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
