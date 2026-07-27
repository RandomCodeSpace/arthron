//! The incremental oracle: after any file event, the store must hold
//! exactly what a cold scan of the same tree would have built.
//!
//! A report can agree while the graph underneath disagrees — a dangling
//! candidate entry, a node one file too many declares, an edge nothing
//! removed. Comparing [`Snapshot`]s is the only assertion that catches
//! those, and it catches them in the stage that introduced the replace
//! rather than three stages later.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::fs;
use std::path::Path;

use arthron::pipeline::scan_go;
use arthron::store::Store;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

const SERVER: &str = concat!(
    "package server\n\n",
    "import (\n\t\"fmt\"\n\t\"example.com/app/util\"\n)\n\n",
    "func Serve() {\n",
    "\tfmt.Println(util.Parse(\"x\"))\n",
    "\thelper()\n",
    "}\n\n",
    "func helper() {}\n",
);

const EXTRA: &str = "package extra\n\nfunc Unused() {}\n";

/// A two-package module whose cross-file link is resolved, plus one
/// external dependency so the external nodes take part in the comparison.
fn fixture(root: &Path) {
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "util/util.go",
        "package util\n\nfunc Parse(s string) string { return s }\n",
    );
    write(root, "server/server.go", SERVER);
}

fn same_map<K: Ord + Debug, V: PartialEq + Debug>(
    what: &str,
    cold: &BTreeMap<K, V>,
    warm: &BTreeMap<K, V>,
) {
    for (k, v) in cold {
        match warm.get(k) {
            None => panic!("{what}: a cold scan holds {k:?} => {v:?}; the warm store does not"),
            Some(w) => assert!(w == v, "{what}: {k:?}\n  cold {v:?}\n  warm {w:?}"),
        }
    }
    for (k, w) in warm {
        assert!(
            cold.contains_key(k),
            "{what}: the warm store holds {k:?} => {w:?}; a cold scan does not",
        );
    }
}

fn same_set<T: Ord + Debug>(what: &str, cold: &BTreeSet<T>, warm: &BTreeSet<T>) {
    let missing: Vec<&T> = cold.difference(warm).collect();
    let extra: Vec<&T> = warm.difference(cold).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{what}: missing from the warm store {missing:?}; left over in it {extra:?}",
    );
}

/// Scan `root` cold into a throwaway store and compare it, whole, with what
/// the incremental scans left in `warm_db`.
fn assert_matches_cold(root: &Path, warm_db: &Path) {
    let cold_dir = tempfile::tempdir().unwrap();
    let cold_db = cold_dir.path().join("cold.redb");
    let cold_report = scan_go(root, &cold_db).expect("cold scan");
    let cold = Store::open(&cold_db)
        .expect("open cold")
        .snapshot()
        .unwrap();

    let warm_store = Store::open(warm_db).expect("open warm");
    let warm = warm_store.snapshot().unwrap();
    let warm_report = warm_store.report().unwrap();

    same_map("files", &cold.files, &warm.files);
    same_map("nodes", &cold.nodes, &warm.nodes);
    same_map("rows", &cold.rows, &warm.rows);
    same_set("edges", &cold.edges, &warm.edges);
    same_map("candidates", &cold.candidates, &warm.candidates);
    assert_eq!(
        cold, warm,
        "the snapshots differ beyond the fields compared"
    );
    assert_eq!(cold_report, warm_report);
}

#[test]
fn cold_equals_warm_on_an_unchanged_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("cold scan");
    // Every hash matches, so the changed set is empty and this event must
    // write nothing — including nothing it then has to delete.
    scan_go(root, &db).expect("warm scan");
    assert_matches_cold(root, &db);
}

#[test]
fn adding_a_file_lands_the_same_state_as_a_cold_scan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");

    write(root, "extra/extra.go", EXTRA);
    scan_go(root, &db).expect("second scan");
    assert_matches_cold(root, &db);
}

#[test]
fn editing_a_file_lands_the_same_state_as_a_cold_scan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");

    // A comment moves every line below it, so every row's `first_line` and
    // every declaration's line has to be rewritten rather than kept.
    write(
        root,
        "server/server.go",
        &SERVER.replace("func Serve() {\n", "// serve one request\nfunc Serve() {\n"),
    );
    scan_go(root, &db).expect("second scan");
    assert_matches_cold(root, &db);
}

#[test]
fn deleting_a_file_lands_the_same_state_as_a_cold_scan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(root, "extra/extra.go", EXTRA);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");

    // The last file of a package: its package node must go with it, and no
    // edge may be left pointing at either.
    fs::remove_file(root.join("extra/extra.go")).unwrap();
    scan_go(root, &db).expect("second scan");
    assert_matches_cold(root, &db);

    let store = Store::open(&db).expect("reopen");
    let nodes = store.snapshot().unwrap().nodes;
    assert!(
        !nodes
            .values()
            .any(|n| format!("{n:?}").contains("app/extra")),
        "nothing of the deleted package survives: {nodes:?}",
    );
}

#[test]
fn renaming_a_file_lands_the_same_state_as_a_cold_scan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");

    // One event that is a delete and an add: the definitions keep their
    // identities (an FQN carries no file), so only the declaration sites
    // move — and the importer, which was never edited, keeps its edge.
    fs::rename(root.join("util/util.go"), root.join("util/parse.go")).unwrap();
    scan_go(root, &db).expect("second scan");
    assert_matches_cold(root, &db);
}

#[test]
fn a_deleted_file_stops_being_a_known_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(root, "extra/extra.go", EXTRA);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");
    assert!(
        Store::open(&db)
            .unwrap()
            .known_files()
            .unwrap()
            .contains(&"extra/extra.go".to_string())
    );

    fs::remove_file(root.join("extra/extra.go")).unwrap();
    scan_go(root, &db).expect("second scan");
    let known = Store::open(&db).unwrap().known_files().unwrap();
    assert!(!known.contains(&"extra/extra.go".to_string()), "{known:?}");
}
