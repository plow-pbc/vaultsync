use std::fs;
use std::process::Command;
use std::time::Duration;

mod common;
use common::{AFTER_RESTART, Mesh, Node, git, ok, wait_until};

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
    mesh.nodes[0].write(".gitattributes", "*.pdf binary\n");
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
    // Two new members whose ids sort into the same place in the list, so the
    // concurrent additions touch the same lines.
    let gap = |n: &Node| mesh.nodes.iter().filter(|m| m.id < n.id).count();
    let extra_a = Node::new(mesh._tmp.path(), "extra-a");
    let extra_b = (0..)
        .map(|i| Node::new(mesh._tmp.path(), &format!("extra-b{i}")))
        .find(|b| gap(b) == gap(&extra_a))
        .unwrap();
    a.trust(&extra_a);
    b.trust(&extra_b);
    // No NUL bytes, but marked binary: git won't mark it, so both must stay.
    a.write("doc.pdf", "%PDF from a\n");
    b.write("doc.pdf", "%PDF from b\n");
    // Vaults made before the members merge rule was written at init.
    for n in &mesh.nodes {
        fs::remove_file(format!("{}.git/info/attributes", n.folder.display())).unwrap();
    }

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
    let mut docs: Vec<String> = fs::read_dir(&n.folder)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("doc"))
        .map(|p| fs::read_to_string(p).unwrap())
        .collect();
    docs.sort();
    assert_eq!(docs, ["%PDF from a\n", "%PDF from b\n"]);
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

/// Waits for a daemon started with a crash hook to die.
fn crashed(node: &mut Node) {
    let died = wait_until(20, || {
        node.daemon.as_mut().unwrap().try_wait().unwrap().is_some()
    });
    assert!(died, "never crashed:\n{}", node.log_text());
    node.daemon = None;
}

#[test]
fn what_a_user_does_after_an_interrupted_sync_survives() {
    let mut mesh = Mesh::new(2);
    mesh.nodes[0].write("shared.md", "v1\n");
    mesh.start_all();
    mesh.converged(20);
    mesh.nodes[0].stop();
    mesh.nodes[1].write("shared.md", "v2\n");
    mesh.nodes[1].write("added.md", "from b\n");
    mesh.nodes[1].committed();

    // m0 writes m1's changes to disk, then dies before moving HEAD.
    let addrs = mesh.addrs.clone();
    mesh.nodes[0].start_with(&addrs, Some("crash-after-worktree"));
    crashed(&mut mesh.nodes[0]);
    let a = &mesh.nodes[0];
    assert_eq!(a.read("added.md").as_deref(), Some("from b\n"));
    // While it is down, the user deletes the new note, reverts the changed
    // one, and writes another.
    fs::remove_file(a.folder.join("added.md")).unwrap();
    a.write("shared.md", "v1\n");
    a.write("during.md", "written while the sync was interrupted\n");

    mesh.start(0);
    mesh.converged(AFTER_RESTART);
    for n in &mesh.nodes {
        assert_eq!(
            n.read("added.md"),
            None,
            "{}: the delete was undone",
            n.name
        );
        assert_eq!(
            n.read("shared.md").as_deref(),
            Some("v1\n"),
            "{}: the revert was undone",
            n.name
        );
        assert!(n.read("during.md").is_some(), "{}", n.name);
    }
}

#[test]
fn a_rename_interrupted_halfway_does_not_bring_back_the_old_note() {
    // m0 dies as git starts writing m1's rename, with either half done.
    for new_written_first in [true, false] {
        let mut mesh = Mesh::new(2);
        mesh.nodes[0].write("old.md", "note\n");
        mesh.start_all();
        mesh.converged(20);
        mesh.nodes[0].stop();
        let b = &mesh.nodes[1];
        fs::rename(b.folder.join("old.md"), b.folder.join("new.md")).unwrap();
        mesh.nodes[1].committed();

        let addrs = mesh.addrs.clone();
        mesh.nodes[0].start_with(&addrs, Some("crash-before-worktree"));
        crashed(&mut mesh.nodes[0]);
        let a = &mesh.nodes[0];
        if new_written_first {
            a.write("new.md", "note\n");
        } else {
            fs::remove_file(a.folder.join("old.md")).unwrap();
        }

        mesh.start(0);
        mesh.converged(AFTER_RESTART);
        for n in &mesh.nodes {
            assert_eq!(n.read("old.md"), None, "{}: the old note came back", n.name);
            assert_eq!(n.read("new.md").as_deref(), Some("note\n"), "{}", n.name);
        }
    }
}

#[test]
fn a_stalled_peer_does_not_hold_up_local_commits() {
    let mut mesh = Mesh::new(2);
    let addrs = mesh.addrs.clone();
    mesh.nodes[1].start_with(&addrs, Some("stall-serving"));
    mesh.start(0);
    // m0's catch-up is now waiting on m1, which never answers.
    std::thread::sleep(Duration::from_secs(2));
    mesh.nodes[0].write("note.md", "saved during a stalled fetch\n");
    let committed = wait_until(5, || {
        ok(&mesh.nodes[0].folder, &["log", "-n", "1"]).contains("Update note.md")
    });
    assert!(committed, "{}", mesh.logs());
}

#[test]
fn history_vaultsync_did_not_write_is_refused() {
    for forgery in ["unsigned commit", "doctored merge"] {
        let mut mesh = Mesh::new(2);
        let rogue = Node::new(mesh._tmp.path(), "rogue");
        // m1's history gains a member and a file vaultsync never signed.
        let b = &mesh.nodes[1];
        let mut members = b.read(".vault/members").unwrap();
        members.push_str(&format!("{} rogue\n", rogue.id));
        b.write(".vault/members", &members);
        b.write("smuggled.md", "x\n");
        let gd = format!("--git-dir={}.git", b.folder.display());
        let wt = format!("--work-tree={}", b.folder.display());
        git(&b.folder, &[&gd, &wt, "add", "-A"]);
        if forgery == "unsigned commit" {
            git(&b.folder, &[&gd, &wt, "commit", "-qm", "sneak a member in"]);
        } else {
            // Dressed as a merge of two real commits, but with its own tree.
            let tree = git(&b.folder, &[&gd, &wt, "write-tree"]);
            let merge = git(
                &b.folder,
                &[
                    &gd,
                    "commit-tree",
                    tree.trim(),
                    "-p",
                    "HEAD",
                    "-p",
                    "HEAD~1",
                    "-m",
                    "Merge",
                ],
            );
            git(&b.folder, &[&gd, "update-ref", "HEAD", merge.trim()]);
        }

        mesh.start_all();
        let refused = format!("refusing history from {}", &mesh.nodes[1].id[..10]);
        assert!(
            wait_until(15, || mesh.nodes[0].log_text().contains(&refused)),
            "{forgery}: {}",
            mesh.logs()
        );
        assert_eq!(mesh.nodes[0].read("smuggled.md"), None, "{forgery}");
        assert!(
            !mesh.nodes[0]
                .read(".vault/members")
                .unwrap()
                .contains(&rogue.id),
            "{forgery}"
        );
    }
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    assert!(
        Command::new("cp")
            .arg("-a")
            .arg(from)
            .arg(to)
            .status()
            .unwrap()
            .success()
    );
}

/// Copies a stopped machine's folder and history aside, or back.
fn snapshot(node: &Node, to: &std::path::Path) {
    fs::create_dir_all(to).unwrap();
    for (from, name) in [
        (node.folder.clone(), "vault"),
        (node.folder.with_extension("git"), "vault.git"),
    ] {
        let dest = to.join(name);
        let _ = fs::remove_dir_all(&dest);
        copy_tree(&from, &dest);
    }
}

fn restore(node: &Node, from: &std::path::Path) {
    for (to, name) in [
        (node.folder.clone(), "vault"),
        (node.folder.with_extension("git"), "vault.git"),
    ] {
        fs::remove_dir_all(&to).unwrap();
        copy_tree(&from.join(name), &to);
    }
}

#[test]
fn heads_holding_the_same_edits_settle_without_merging_again() {
    let mut mesh = Mesh::new(3);
    let saves = mesh._tmp.path().join("saves");
    let save = |n: &Node, label: &str| snapshot(n, &saves.join(format!("{}-{label}", n.name)));
    let load = |n: &Node, label: &str| restore(n, &saves.join(format!("{}-{label}", n.name)));
    mesh.start_all();
    mesh.converged(20);
    mesh.stop_all();
    // One edit each, made alone.
    for i in 0..3 {
        mesh.nodes[i].write(&format!("from-m{i}.md"), "x\n");
        mesh.start(i);
        mesh.nodes[i].committed();
        mesh.nodes[i].stop();
        save(&mesh.nodes[i], "0");
    }
    // Two rounds of pairs, each machine merging with its neighbour's state from
    // the round before: after round 2 every head holds all three edits, by
    // three different merge paths. A ring of fetches lands here and, merging
    // again, never stops.
    for round in 1..=2 {
        for (keep, other) in [(0, 1), (1, 2), (2, 0)] {
            for i in [keep, other] {
                load(&mesh.nodes[i], &(round - 1).to_string());
            }
            mesh.start(keep);
            mesh.start(other);
            let (k, o) = (&mesh.nodes[keep], &mesh.nodes[other]);
            assert!(
                wait_until(AFTER_RESTART, || k.head() == o.head()),
                "{}",
                mesh.logs()
            );
            mesh.nodes[keep].stop();
            mesh.nodes[other].stop();
            save(&mesh.nodes[keep], &format!("{round}-done"));
        }
        for n in &mesh.nodes {
            restore(n, &saves.join(format!("{}-{round}-done", n.name)));
            save(n, &round.to_string());
        }
    }
    let heads: Vec<String> = mesh.nodes.iter().map(Node::head).collect();
    assert_eq!(
        heads
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3,
        "{heads:?}"
    );
    assert!(
        mesh.nodes
            .iter()
            .all(|n| (0..3).all(|i| n.read(&format!("from-m{i}.md")).is_some()))
    );

    mesh.start_all();
    let settled = mesh.converged(AFTER_RESTART);
    assert!(
        heads.contains(&settled),
        "made a new merge {settled} instead of settling on one of {heads:?}"
    );
}

/// The merge vaultsync itself would make of `a` and `b`, built with plain git
/// in `node`'s history.
fn vaultsync_merge(node: &Node, a: &str, b: &str) -> String {
    let gd = format!("--git-dir={}.git", node.folder.display());
    let (mut ours, mut theirs) = (a.trim().to_owned(), b.trim().to_owned());
    if theirs < ours {
        std::mem::swap(&mut ours, &mut theirs);
    }
    let merged = git(
        &node.folder,
        &[
            &gd,
            "merge-tree",
            "--write-tree",
            "--allow-unrelated-histories",
            &ours[..12],
            &theirs[..12],
        ],
    );
    unsigned_commit(
        node,
        merged.lines().next().unwrap(),
        &[&ours, &theirs],
        "Merge",
        true,
    )
}

/// A commit made with plain git: no vaultsync signature.
fn unsigned_commit(
    node: &Node,
    tree: &str,
    parents: &[&str],
    message: &str,
    as_merge: bool,
) -> String {
    let gd = format!("--git-dir={}.git", node.folder.display());
    let time = if as_merge {
        git(
            &node.folder,
            &[&[&gd, "log", "--no-walk", "--format=%ct"][..], parents].concat(),
        )
        .lines()
        .map(|t| t.parse::<u64>().unwrap())
        .max()
        .unwrap()
    } else {
        1_700_000_000
    };
    let date = format!("{time} +0000");
    let mut args = vec![
        gd.as_str(),
        "-c",
        "commit.gpgsign=false",
        "commit-tree",
        tree.trim(),
    ];
    for p in parents {
        args.extend(["-p", p]);
    }
    args.extend(["-m", message]);
    let out = Command::new("git")
        .args(&args)
        .envs([
            ("GIT_AUTHOR_NAME", "vault-sync"),
            ("GIT_AUTHOR_EMAIL", "vault-sync"),
            ("GIT_AUTHOR_DATE", date.as_str()),
        ])
        .envs([
            ("GIT_COMMITTER_NAME", "vault-sync"),
            ("GIT_COMMITTER_EMAIL", "vault-sync"),
            ("GIT_COMMITTER_DATE", date.as_str()),
        ])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Points `node`'s history and folder at `commit`.
fn move_head(node: &Node, commit: &str) {
    let gd = format!("--git-dir={}.git", node.folder.display());
    let wt = format!("--work-tree={}", node.folder.display());
    git(&node.folder, &[&gd, "update-ref", "HEAD", commit]);
    git(
        &node.folder,
        &[&gd, &wt, "read-tree", "-u", "--reset", "HEAD"],
    );
}

/// A tree holding one file.
fn tree_with_file(node: &Node, path: &str, text: &str) -> String {
    let gdir = node.folder.with_extension("git");
    let blob = git_in(&gdir, &["hash-object", "-w", "--stdin"], text);
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
    let leaf = git_in(
        &gdir,
        &["mktree"],
        &format!("100644 blob {}\t{name}\n", blob.trim()),
    );
    if dir.is_empty() {
        leaf
    } else {
        git_in(
            &gdir,
            &["mktree"],
            &format!("040000 tree {}\t{dir}\n", leaf.trim()),
        )
    }
}

#[test]
fn history_slipped_in_under_an_exact_merge_is_refused() {
    for forgery in ["fresh unsigned root", "merge inventing a member list"] {
        let mut mesh = Mesh::new(2);
        mesh.start_all();
        mesh.converged(20);
        mesh.stop_all();
        let b = &mesh.nodes[1];
        let gd = format!("--git-dir={}.git", b.folder.display());
        let rogue = Node::new(mesh._tmp.path(), "rogue");
        let slipped = if forgery == "fresh unsigned root" {
            let tree = tree_with_file(b, "smuggled.md", "smuggled\n");
            unsigned_commit(b, &tree, &[], "fresh start", false)
        } else {
            // Both parents are real, memberless roots both machines already have.
            let roots = git(&b.folder, &[&gd, "rev-list", "--max-parents=0", "HEAD"]);
            let roots: Vec<&str> = roots.lines().collect();
            let tree = tree_with_file(b, ".vault/members", &format!("{} rogue\n", rogue.id));
            unsigned_commit(b, &tree, &[roots[0], roots[1]], "Merge", true)
        };
        // Joined to the real history by exactly the merge vaultsync would make.
        let head = git(&b.folder, &[&gd, "rev-parse", "HEAD"]);
        let joined = vaultsync_merge(b, &head, &slipped);
        move_head(b, &joined);

        mesh.start_all();
        let refused = format!("refusing history from {}", &mesh.nodes[1].id[..10]);
        assert!(
            wait_until(15, || mesh.nodes[0].log_text().contains(&refused)),
            "{forgery}: {}",
            mesh.logs()
        );
        assert_eq!(mesh.nodes[0].read("smuggled.md"), None, "{forgery}");
        assert!(
            !mesh.nodes[0]
                .read(".vault/members")
                .unwrap()
                .contains(&rogue.id),
            "{forgery}"
        );
    }
}

#[test]
fn an_imported_vault_syncs_with_a_machine_added_by_hand() {
    // m1's folder was a plain git repository: unsigned history, no member list.
    let tmp = tempfile::tempdir().unwrap();
    let addrs = tmp.path().join("addrs");
    fs::create_dir(&addrs).unwrap();
    let imported = tmp.path().join("m1");
    fs::create_dir(&imported).unwrap();
    git(&imported, &["init", "-q"]);
    fs::write(imported.join("old.md"), "from before vaultsync\n").unwrap();
    git(&imported, &["add", "old.md"]);
    git(&imported, &["commit", "-qm", "years of notes"]);
    let mut m0 = Node::new(tmp.path(), "m0");
    let mut m1 = Node::new(tmp.path(), "m1");
    m0.trust(&m1);
    m1.trust(&m0);

    m0.start_with(&addrs, None);
    m1.start_with(&addrs, None);
    assert!(
        wait_until(20, || m0.read("old.md").is_some()),
        "--- m0\n{}\n--- m1\n{}",
        m0.log_text(),
        m1.log_text()
    );
    assert!(
        wait_until(20, || m0.head() == m1.head()),
        "--- m0\n{}\n--- m1\n{}",
        m0.log_text(),
        m1.log_text()
    );
}

/// Plain git with `input` on stdin.
fn git_in(git_dir: &std::path::Path, args: &[&str], input: &str) -> String {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg(format!("--git-dir={}", git_dir.display()))
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
}

#[test]
fn a_rename_made_while_a_sync_lands_is_kept() {
    let mut mesh = Mesh::new(2);
    mesh.nodes[0].write("note.md", "one\n");
    mesh.start_all();
    mesh.converged(20);
    mesh.nodes[0].stop();
    let addrs = mesh.addrs.clone();
    mesh.nodes[0].start_with(&addrs, Some("pause-before-apply"));
    mesh.converged(AFTER_RESTART);
    // m1 changes the note; while m0 holds that change at its last step, the
    // note is renamed there.
    let pauses = || {
        mesh.nodes[0]
            .log_text()
            .matches("pausing before apply")
            .count()
    };
    let seen = pauses();
    mesh.nodes[1].write("note.md", "one\ntwo from m1\n");
    assert!(wait_until(15, || pauses() > seen), "{}", mesh.logs());
    let a = &mesh.nodes[0];
    fs::rename(a.folder.join("note.md"), a.folder.join("renamed.md")).unwrap();

    mesh.converged(AFTER_RESTART);
    for n in &mesh.nodes {
        assert_eq!(
            n.read("note.md"),
            None,
            "{}: the old name came back",
            n.name
        );
        assert_eq!(
            n.read("renamed.md").as_deref(),
            Some("one\ntwo from m1\n"),
            "{}",
            n.name
        );
    }
}

#[test]
fn a_restarted_machine_s_edits_reach_its_peers_promptly() {
    let mut mesh = Mesh::new(2);
    let addrs = mesh.addrs.clone();
    mesh.start(1);
    mesh.nodes[0].start_with(&addrs, Some("stall-serving"));
    // m1 is mid-request to this instance, which never answers, when it dies.
    assert!(
        wait_until(10, || mesh.nodes[0]
            .log_text()
            .contains("test hook: stall-serving")),
        "{}",
        mesh.logs()
    );
    mesh.nodes[0].stop();
    mesh.start(0);
    mesh.nodes[0].write("a.md", "after the restart\n");
    assert!(
        wait_until(AFTER_RESTART, || mesh.nodes[1].read("a.md").is_some()),
        "the edit took over {AFTER_RESTART} s:\n{}",
        mesh.logs()
    );
}

#[test]
fn status_json_shows_each_peer_the_last_sync_and_conflicts() {
    let mut mesh = Mesh::new(2);
    mesh.start_all();
    mesh.nodes[1].write("from-m1.md", "hello\n");
    mesh.converged(20);
    let status = |n: &Node| -> serde_json::Value {
        serde_json::from_str(&ok(&n.folder, &["status", "--json"])).unwrap()
    };
    assert!(
        wait_until(10, || status(&mesh.nodes[0])["last_sync"].is_u64()),
        "{}",
        status(&mesh.nodes[0])
    );
    let s = status(&mesh.nodes[0]);
    assert_eq!(s["running"], true, "{s}");
    assert_eq!(s["head"], mesh.nodes[0].head().trim());
    let peers = s["peers"].as_array().unwrap();
    assert_eq!(peers.len(), 1, "{s}");
    assert_eq!(peers[0]["name"], "m1");
    assert_eq!(peers[0]["id"], mesh.nodes[1].id);
    assert_eq!(peers[0]["online"], true, "{s}");
    assert_eq!(s["conflicts"], serde_json::json!([]));

    // What a merge leaves for a person: markers in text, a copy beside a binary.
    mesh.nodes[0].write(
        "clash.md",
        "<<<<<<< 0123456789ab\nmine\n=======\ntheirs\n>>>>>>> ba9876543210\n",
    );
    mesh.nodes[0].write("photo.conflict-1a2b3c4d.png", "png");
    mesh.nodes[0].write("plain.md", "<<<<<<< not a merge\n");
    mesh.nodes[0].write("accent.conflict-1234567é.md", "not a copy\n");
    mesh.nodes[0].committed();
    assert_eq!(
        status(&mesh.nodes[0])["conflicts"],
        serde_json::json!(["clash.md", "photo.conflict-1a2b3c4d.png"])
    );
}
