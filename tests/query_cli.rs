//! End-to-end for the `query` subcommand.
//!
//! This drives the built binary rather than the library, because the exit
//! code and the printed columns *are* the product surface: an answer a person
//! cannot read, or a script cannot branch on, is not an answer.
//!
//! Library-level coverage of the same three verbs lives in `tests/query.rs`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// The same three-package module `tests/query.rs` uses:
/// `api#Handle → server#Serve → util#Parse`, with `server#helper` a second
/// caller of `util#Parse`.
fn fixture(root: &Path) {
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "util/util.go",
        "package util\n\nfunc Parse(s string) string { return s }\n",
    );
    write(
        root,
        "server/server.go",
        concat!(
            "package server\n\n",
            "import \"example.com/app/util\"\n\n",
            "func Serve() {\n",
            "\tutil.Parse(\"x\")\n",
            "\thelper()\n",
            "}\n\n",
            "func helper() {\n",
            "\tutil.Parse(\"y\")\n",
            "}\n",
        ),
    );
    write(
        root,
        "api/api.go",
        concat!(
            "package api\n\n",
            "import \"example.com/app/server\"\n\n",
            "func Handle() {\n",
            "\tserver.Serve()\n",
            "}\n",
        ),
    );
}

fn arthron(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args(args)
        .output()
        .expect("running the arthron binary")
}

/// Scan a fixture tree and hand back the store path a query should read.
fn scanned(root: &Path) -> std::path::PathBuf {
    fixture(root);
    let db = root.join("graph.redb");
    let out = arthron(&["scan", root.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    db
}

fn query(db: &Path, args: &[&str]) -> (String, String, i32) {
    let mut full = vec!["query"];
    full.extend_from_slice(args);
    full.push("--db");
    full.push(db.to_str().unwrap());
    let out = arthron(&full);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().expect("the process exited normally"),
    )
}

#[test]
fn def_prints_the_record_and_its_declaration_site() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["def", "example.com/app/util#Parse"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("definition   example.com/app/util#Parse"),
        "{stdout}"
    );
    assert!(stdout.contains("kind         function"), "{stdout}");
    assert!(stdout.contains("declared     util/util.go:3"), "{stdout}");
}

#[test]
fn refs_prints_every_resolved_site() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["refs", "Parse"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("2 row(s), 2 occurrence(s)"), "{stdout}");
    assert!(stdout.contains("server/server.go:6"), "{stdout}");
    assert!(stdout.contains("server/server.go:11"), "{stdout}");
    assert!(stdout.contains("example.com/app/server#helper"), "{stdout}");
    assert!(stdout.contains("resolved"), "{stdout}");
}

#[test]
fn refs_on_a_node_nothing_calls_says_so_and_still_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["refs", "Handle"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("no stored row resolves here"), "{stdout}");
}

#[test]
fn impact_prints_one_block_per_hop() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["impact", "Parse"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("depth 1      2 node(s)"), "{stdout}");
    assert!(stdout.contains("depth 2      1 node(s)"), "{stdout}");
    assert!(stdout.contains("example.com/app/api#Handle"), "{stdout}");
    assert!(
        !stdout.contains("truncated"),
        "the closure is exhausted: {stdout}"
    );
}

#[test]
fn impact_declares_the_bound_it_stopped_at() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["impact", "Parse", "--depth", "1"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("depth 1      2 node(s)"), "{stdout}");
    assert!(!stdout.contains("depth 2"), "{stdout}");
    assert!(stdout.contains("truncated"), "{stdout}");
}

#[test]
fn an_ambiguous_name_lists_the_candidates_and_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    write(
        dir.path(),
        "text/text.go",
        "package text\n\nfunc Parse(s string) string { return s }\n",
    );
    let db = dir.path().join("graph.redb");
    let out = arthron(&[
        "scan",
        dir.path().to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
    ]);
    assert!(out.status.success());

    let (stdout, _, code) = query(&db, &["def", "Parse"]);
    assert_eq!(
        code, 1,
        "ambiguity is an answer, and not a zero one: {stdout}"
    );
    assert!(
        stdout.contains("ambiguous: 2 matches for \"Parse\""),
        "{stdout}"
    );
    // The candidates are on stdout so they can be piped into the re-run.
    assert!(stdout.contains("example.com/app/text#Parse"), "{stdout}");
    assert!(stdout.contains("example.com/app/util#Parse"), "{stdout}");
}

#[test]
fn a_name_that_is_not_there_exits_one_and_says_only_that() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, _, code) = query(&db, &["def", "NoSuchThing"]);
    assert_eq!(code, 1);
    assert!(stdout.contains("no match for \"NoSuchThing\""), "{stdout}");
}

#[test]
fn an_absent_store_is_a_usage_error_and_not_a_new_empty_graph() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("never-scanned.redb");

    let (_, stderr, code) = query(&db, &["def", "Parse"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("never-scanned.redb"), "{stderr}");
    // The whole point of the read-only open: a query must not conjure the
    // store it failed to find.
    assert!(
        !db.exists(),
        "a query created a store it was only asked to read"
    );
}
