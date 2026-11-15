use std::fs;
use std::time::Duration;

mod common;
use common::{AFTER_RESTART, Mesh, Node, git, ok, vaultsync, wait_until};

fn history(node: &Node) -> String {
    let gd = format!("--git-dir={}.git", node.folder.display());
    git(&node.folder, &[&gd, "rev-list", "HEAD"])
}

#[test]
fn a_new_machine_joins_with_full_history_and_syncs_with_everyone() {
    let mut mesh = Mesh::new(2);
    mesh.nodes[0].write("people/ada.md", "# Ada\n");
    mesh.start_all();
    mesh.converged(20);

    let code = mesh.nodes[0].invite(600);
    let root = mesh._tmp.path().to_owned();
    let mut joiner = Node::join(&root, "m2", &code, &mesh.addrs).unwrap();

    assert_eq!(
        joiner.head(),
        mesh.nodes[0].head(),
        "joined at the inviter's head"
    );
    assert_eq!(history(&joiner), history(&mesh.nodes[0]));
    assert_eq!(joiner.read("people/ada.md").as_deref(), Some("# Ada\n"));
    assert!(joiner.read(".vault/members").unwrap().contains(&joiner.id));

    // m1 learns of the newcomer from m0's history, and takes its edits.
    joiner.start_with(&mesh.addrs, None);
    joiner.write("from-m2.md", "hello from the new machine\n");
    let everywhere = || mesh.nodes.iter().all(|n| n.read("from-m2.md").is_some());
    assert!(
        wait_until(20, everywhere),
        "{}\n--- m2\n{}",
        mesh.logs(),
        joiner.log_text()
    );
}

#[test]
fn an_invite_works_once_and_not_after_it_expires() {
    let mut mesh = Mesh::new(1);
    mesh.start(0);
    let root = mesh._tmp.path().to_owned();
    let used = mesh.nodes[0].invite(600);
    Node::join(&root, "first", &used, &mesh.addrs).unwrap();

    let reused = Node::join(&root, "second", &used, &mesh.addrs)
        .err()
        .expect("reuse must fail");
    assert!(reused.contains("already used"), "{reused}");
    assert!(
        !root.join("second").exists() && !root.join("second.git").exists(),
        "a failed join leaves nothing behind"
    );

    let stale = mesh.nodes[0].invite(1);
    std::thread::sleep(Duration::from_secs(2));
    let expired = Node::join(&root, "late", &stale, &mesh.addrs)
        .err()
        .expect("an expired code must fail");
    assert!(expired.contains("expired"), "{expired}");
    assert!(!root.join("late").exists());

    let log = mesh.nodes[0].log_text();
    assert_eq!(log.matches("refused join").count(), 2, "{log}");
    assert!(
        !log.contains(&used) && !log.contains(&stale),
        "codes must never reach a log"
    );
}

#[test]
fn joining_into_a_folder_with_files_stops() {
    let mesh = Mesh::new(1);
    let code = mesh.nodes[0].invite(600);
    let folder = mesh._tmp.path().join("busy");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("mine.md"), "keep\n").unwrap();

    let out = vaultsync(
        mesh._tmp.path(),
        &["join", &code, "--folder", folder.to_str().unwrap()],
    );

    assert!(!out.status.success());
    let message = String::from_utf8_lossy(&out.stderr);
    assert!(message.contains("already has files"), "{message}");
    assert_eq!(
        fs::read_to_string(folder.join("mine.md")).unwrap(),
        "keep\n"
    );
}

#[test]
fn a_removed_machine_is_refused_by_every_member_the_removal_reaches() {
    let mut mesh = Mesh::new(3);
    mesh.start_all();
    mesh.converged(20);
    // m1 is offline when m2 is removed.
    mesh.nodes[1].stop();
    ok(&mesh.nodes[0].folder, &["remove", "m2"]);
    mesh.nodes[2].write("after.md", "written after the removal\n");

    let refused = format!("refused connection from {}", &mesh.nodes[2].id[..10]);
    assert!(
        wait_until(15, || mesh.nodes[0].log_text().contains(&refused)),
        "{}",
        mesh.logs()
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(mesh.nodes[0].read("after.md"), None);

    // m1 comes back, syncs with m0, and from then on refuses m2 too.
    mesh.start(1);
    assert!(
        wait_until(AFTER_RESTART, || mesh.nodes[1]
            .log_text()
            .contains(&refused)),
        "{}",
        mesh.logs()
    );
    let members = mesh.nodes[1].read(".vault/members").unwrap();
    assert!(
        members.contains(&format!("removed {} m2", mesh.nodes[2].id)),
        "{members}"
    );

    // And it stays removed.
    let again = vaultsync(
        &mesh.nodes[0].folder,
        &["member", "add", &mesh.nodes[2].id, "--name", "m2"],
    );
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("was removed"));
}

#[test]
fn a_joiner_cannot_smuggle_lines_into_the_member_list() {
    let mut mesh = Mesh::new(1);
    mesh.start(0);
    let code = mesh.nodes[0].invite(600);
    let folder = mesh._tmp.path().join("sly");
    let name = format!("sly\nremoved {} m0", mesh.nodes[0].id);
    let addrs = mesh.addrs.to_str().unwrap().to_owned();

    let out = vaultsync(
        mesh._tmp.path(),
        &[
            "join",
            &code,
            "--folder",
            folder.to_str().unwrap(),
            "--machine",
            &name,
            "--addr-dir",
            &addrs,
        ],
    );

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let members = mesh.nodes[0].read(".vault/members").unwrap();
    assert!(!members.contains("removed"), "{members}");
    assert!(members.lines().any(|l| l.ends_with(" sly")), "{members}");
}

#[test]
fn a_removal_wins_over_a_concurrent_member_change() {
    let mut mesh = Mesh::new(3);
    mesh.start_all();
    mesh.converged(20);
    mesh.nodes[1].stop();
    ok(&mesh.nodes[0].folder, &["remove", "m2"]);
    // Offline, m1 adds a member listed right beside m2, so both edits touch
    // the same lines and the merge keeps m2's line next to its tombstone.
    let m2 = mesh.nodes[2].id.clone();
    let ids: Vec<String> = mesh.nodes.iter().map(|n| n.id.clone()).collect();
    // No listed id sorts between it and m2.
    let beside = |x: &str| {
        let (lo, hi) = if x < m2.as_str() {
            (x, m2.as_str())
        } else {
            (m2.as_str(), x)
        };
        !ids.iter().any(|id| lo < id.as_str() && id.as_str() < hi)
    };
    let extra = (0..)
        .map(|i| Node::new(mesh._tmp.path(), &format!("extra{i}")))
        .find(|n| beside(&n.id))
        .unwrap();
    mesh.nodes[1].trust(&extra);

    mesh.start(1);
    let refused = format!("refused connection from {}", &m2[..10]);
    assert!(
        wait_until(AFTER_RESTART, || mesh.nodes[1]
            .log_text()
            .contains(&refused)),
        "{}",
        mesh.logs()
    );
    let members = mesh.nodes[1].read(".vault/members").unwrap();
    assert!(
        members.contains(&extra.id) && members.contains(&format!("removed {m2}")),
        "{members}"
    );
}

#[test]
fn join_never_touches_history_reached_through_a_symlink() {
    // m0's folder is empty, but its history sits beside it.
    let mesh = Mesh::new(1);
    let m0 = &mesh.nodes[0];
    let head = m0.head();
    let alias = mesh._tmp.path().join("alias");
    std::os::unix::fs::symlink(&m0.folder, &alias).unwrap();
    let code = m0.invite(600);

    let out = vaultsync(
        mesh._tmp.path(),
        &["join", &code, "--folder", alias.to_str().unwrap()],
    );

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already has files or history"));
    assert_eq!(m0.head(), head, "m0's history must be untouched");
}

#[test]
fn rollback_never_changes_the_member_list() {
    let mesh = Mesh::new(1);
    let m0 = &mesh.nodes[0];
    let start = ok(&m0.folder, &["log"])
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let gone = Node::new(mesh._tmp.path(), "gone");
    m0.write("notes.md", "later\n");
    m0.trust(&gone);
    ok(&m0.folder, &["remove", "gone"]);

    // Back to before gone was ever added: the notes go, the tombstone stays.
    ok(&m0.folder, &["rollback", &start]);
    assert_eq!(m0.read("notes.md"), None);
    let members = m0.read(".vault/members").unwrap();
    assert!(
        members.contains(&format!("removed {} gone", gone.id)),
        "{members}"
    );

    // Nor when the selection takes it in.
    for path in [".vault/members", "."] {
        let _ = vaultsync(&m0.folder, &["rollback", &start, path]);
        let after = m0.read(".vault/members").unwrap();
        assert!(
            after.contains(&format!("removed {} gone", gone.id)),
            "{path}: {after}"
        );
    }
}

#[test]
fn a_fetch_that_began_before_a_removal_gets_nothing_after_it() {
    let mut mesh = Mesh::new(2);
    let addrs = mesh.addrs.clone();
    mesh.nodes[0].start_with(&addrs, Some("pause-before-bundle"));
    mesh.start(1);
    mesh.converged(AFTER_RESTART);
    // The moment one of m1's fetches is held mid-request, remove m1 and write.
    let pauses = || {
        mesh.nodes[0]
            .log_text()
            .matches("pausing before the bundle")
            .count()
    };
    let seen = pauses();
    assert!(wait_until(15, || pauses() > seen), "{}", mesh.logs());
    ok(&mesh.nodes[0].folder, &["remove", "m1"]);
    mesh.nodes[0].write("secret.md", "after m1 was removed\n");

    std::thread::sleep(Duration::from_secs(8));
    assert_eq!(mesh.nodes[1].read("secret.md"), None, "{}", mesh.logs());
    // Not even its contents reached m1's history.
    let gd0 = format!("--git-dir={}.git", mesh.nodes[0].folder.display());
    let blob = git(
        &mesh.nodes[0].folder,
        &[&gd0, "rev-parse", "HEAD:secret.md"],
    );
    let in_m1 = std::process::Command::new("git")
        .arg(format!("--git-dir={}.git", mesh.nodes[1].folder.display()))
        .args(["cat-file", "-e", blob.trim()])
        .status()
        .unwrap();
    assert!(!in_m1.success(), "m1 received the secret's contents");
    assert!(
        mesh.nodes[0].log_text().contains("is no longer a member"),
        "{}",
        mesh.logs()
    );
}

#[test]
fn a_failed_join_keeps_files_it_did_not_write() {
    let mut mesh = Mesh::new(1);
    let addrs = mesh.addrs.clone();
    mesh.nodes[0].start_with(&addrs, Some("stall-serving"));
    let code = mesh.nodes[0].invite(600);
    let folder = mesh._tmp.path().join("newcomer");
    let mut join = std::process::Command::new(env!("CARGO_BIN_EXE_vaultsync"))
        .args([
            "join",
            &code,
            "--folder",
            folder.to_str().unwrap(),
            "--addr-dir",
            addrs.to_str().unwrap(),
        ])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // While the join waits on the inviter, an editor saves into the folder.
    assert!(wait_until(10, || folder.with_extension("git").exists()));
    fs::write(folder.join("typed-meanwhile.md"), "keep me\n").unwrap();
    mesh.nodes[0].stop();

    assert!(
        wait_until(90, || join.try_wait().unwrap().is_some()),
        "join never gave up"
    );
    assert!(!join.wait().unwrap().success());
    assert_eq!(
        fs::read_to_string(folder.join("typed-meanwhile.md")).unwrap(),
        "keep me\n"
    );
    assert!(
        !folder.with_extension("git").exists(),
        "join's own history is cleaned up"
    );
}
