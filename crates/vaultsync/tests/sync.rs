use std::fs;
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
