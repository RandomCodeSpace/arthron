//! A scan measures the tree it was pointed at, and a symbolic link is not a
//! hole in that tree.
//!
//! The walk is built with `follow_links(false)`, which reads as "links are not
//! followed" and is only half true: the walk hands back the link itself, and
//! the `is_file` check that follows *does* follow it. So a link inside the
//! repository pointing at `/etc/…` or at a sibling checkout was read, and the
//! definitions it declared were stored under a repo-relative key — as though
//! that file were part of this repository. Every name in the graph is then a
//! claim about a tree that does not contain it.
//!
//! The rule is about where the target lands, not about links: a link whose
//! target is inside the repository is an ordinary file of that repository and
//! is still read. The measured corpus depends on it — `aeson` links two
//! modules into a sub-package from its own `src/` — so "refuse every symlink"
//! would be a different bug and a moved baseline.
//!
//! Unix only: `std::os::unix::fs::symlink` is how a link is made here, and the
//! whole file is inexpressible without it rather than merely skipped.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use arthron::pipeline::scan_repo;
use arthron::query::NameIndex;
use arthron::store::{ReadStore, Report};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    fs::write(path, content).expect("write");
}

/// Whether the graph holds a node this name ends.
fn holds(db: &Path, fqn: &str) -> bool {
    let store = ReadStore::open(db).expect("the store opens read-only");
    let index = NameIndex::build(&store).expect("the index builds");
    !index.lookup(fqn).matches.is_empty()
}

fn errored(report: &Report, path: &str) -> Option<String> {
    report
        .file_errors
        .iter()
        .find(|e| e.path == path)
        .map(|e| e.message.clone())
}

#[test]
fn a_symlink_whose_target_is_outside_the_repository_is_not_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let outside = dir.path().join("outside");
    write(&repo, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(&repo, "main.go", "package main\n\nfunc main() {}\n");
    write(
        &outside,
        "leak.go",
        "package main\n\nfunc SecretOutsideTheRepo() {}\n",
    );
    // Both spellings of the same escape: an absolute target, and a relative
    // one that climbs out.
    symlink(outside.join("leak.go"), repo.join("absolute.go")).expect("symlink");
    symlink("../outside/leak.go", repo.join("relative.go")).expect("symlink");

    let db = repo.join("graph.redb");
    let report = scan_repo(&repo, &db).expect("the scan runs");
    assert!(
        !holds(&db, "example.com/app#SecretOutsideTheRepo"),
        "a file outside the repository was stored as a file of it",
    );
    // …and it is not read *quietly*: a file the walk reached and did not use
    // is named, exactly as an unreadable one is.
    for link in ["absolute.go", "relative.go"] {
        let said = errored(&report, link)
            .unwrap_or_else(|| panic!("{link} vanished silently: {:?}", report.file_errors));
        assert!(
            said.contains("outside"),
            "the entry has to say why the file was left: {said}",
        );
    }
}

#[test]
fn a_symlink_whose_target_is_inside_the_repository_is_still_read() {
    // The corpus depends on this: `aeson` links two of its own modules into a
    // sub-package, and refusing every symlink would drop them and move a
    // committed baseline.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    write(&repo, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        &repo,
        "real/r.go",
        "package real\n\nfunc R() {}\n\nfunc use() { R() }\n",
    );
    fs::create_dir_all(repo.join("mirror")).expect("mkdir");
    symlink("../real/r.go", repo.join("mirror/m.go")).expect("symlink");

    let db = repo.join("graph.redb");
    let report = scan_repo(&repo, &db).expect("the scan runs");
    assert!(
        holds(&db, "example.com/app/real#R"),
        "the real file stopped being read",
    );
    assert!(
        holds(&db, "example.com/app/mirror#R"),
        "a link inside the repository stopped being read",
    );
    assert_eq!(
        errored(&report, "mirror/m.go"),
        None,
        "an in-repository link was refused: {:?}",
        report.file_errors,
    );
}

#[test]
fn a_link_that_climbs_out_and_back_in_is_inside() {
    // Containment is where the target lands, not how it is spelt. `..` that
    // returns is not an escape, and refusing it would be refusing an ordinary
    // file of the repository.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    write(&repo, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        &repo,
        "real/r.go",
        "package real\n\nfunc R() {}\n\nfunc use() { R() }\n",
    );
    fs::create_dir_all(repo.join("mirror")).expect("mkdir");
    symlink("../../repo/real/r.go", repo.join("mirror/m.go")).expect("symlink");

    let db = repo.join("graph.redb");
    let report = scan_repo(&repo, &db).expect("the scan runs");
    assert!(
        holds(&db, "example.com/app/mirror#R"),
        "{:?}",
        report.file_errors
    );
}

#[test]
fn a_dangling_link_is_no_more_and_no_less_than_it_was() {
    // It points at nothing, so nothing is read and nothing is claimed about
    // where it would have pointed. The escape check must not turn "cannot
    // answer" into "outside".
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    write(&repo, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        &repo,
        "a.go",
        "package app\n\nfunc A() {}\n\nfunc use() { A() }\n",
    );
    symlink("nowhere.go", repo.join("dangling.go")).expect("symlink");

    let db = repo.join("graph.redb");
    let report = scan_repo(&repo, &db).expect("the scan runs");
    assert!(holds(&db, "example.com/app#A"), "the real file was lost");
    assert_eq!(
        errored(&report, "dangling.go"),
        None,
        "{:?}",
        report.file_errors
    );
}
