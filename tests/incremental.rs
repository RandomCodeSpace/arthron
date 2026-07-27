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

use arthron::UnresolvedReason;
use arthron::model::{Domain, NodeId, RefKind, node_id, reason_code};
use arthron::pipeline::scan_go;
use arthron::store::{Store, StoredOutcome};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// The identity a Go FQN hashes to.
fn go(fqn: &str) -> NodeId {
    node_id(Domain::Go, fqn)
}

/// The single stored outcome for one reference site.
///
/// Asserting on the row rather than on the tally is what makes these tests
/// about *this* reference: a tally can stay put while two references swap
/// outcomes.
fn outcome(db: &Path, file: &str, raw_target: &str) -> StoredOutcome {
    let rows = Store::open(db).expect("open").snapshot().unwrap().rows;
    let mut found: Vec<StoredOutcome> = rows
        .iter()
        .filter(|(key, _)| key.file == file && key.raw_target == raw_target)
        .map(|(_, row)| row.outcome.clone())
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one `{raw_target}` row in {file}, found {found:?}",
    );
    found.pop().expect("one row")
}

fn unresolved(reason: UnresolvedReason) -> StoredOutcome {
    StoredOutcome::Unresolved(reason_code(&reason))
}

fn calls(db: &Path, src: &str, dst: &str) -> bool {
    Store::open(db)
        .expect("open")
        .has_edge(&go(src), &go(dst), RefKind::Call.code())
        .expect("edge lookup")
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

// ---------------------------------------------------------------------
// Candidate invalidation: an edit that adds or removes a definition has to
// reach the references in files nobody touched.
//
// Every test below edits one file and asserts about another. If the affected
// set ever degenerates to "only the changed files", each of them fails —
// which is the point, because the degenerate version passes any test whose
// caller happens to sit in the file that changed.
// ---------------------------------------------------------------------

/// Calls a name its own package does not define — yet.
const CALLER: &str = "package server\n\nfunc Call() {\n\tMissing()\n}\n";

const MISSING: &str = "package server\n\nfunc Missing() {}\n";

#[test]
fn renaming_the_module_lands_a_cold_scans_store() {
    // `go.mod`'s module directive is the root of every FQN in the graph: a
    // package's import path is `{module}/{dir}`, and every node id hashes
    // that path. Rewriting the directive renames every node in the store
    // while not one `.go` file's bytes move — so a scan that hashes only
    // source files sees an empty changed set and leaves the whole graph
    // under the old module path.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("cold scan");
    assert!(calls(
        &db,
        "example.com/app/server.Serve",
        "example.com/app/util.Parse",
    ));

    write(root, "go.mod", "module example.com/renamed\n\ngo 1.22\n");
    scan_go(root, &db).expect("scan after the module rename");

    assert_matches_cold(root, &db);
}

#[test]
fn adding_a_definition_repoints_an_unresolved_caller_in_an_unchanged_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(root, "server/caller.go", CALLER);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");
    assert_eq!(
        outcome(&db, "server/caller.go", "Missing"),
        unresolved(UnresolvedReason::NoMatchingDefinition),
        "nothing declares it yet",
    );

    // A new file, in a package the caller already belongs to. The caller is
    // not rewritten, so its hash still matches and the changed set holds one
    // file it does not appear in.
    write(root, "server/missing.go", MISSING);
    scan_go(root, &db).expect("second scan");

    assert_eq!(
        outcome(&db, "server/caller.go", "Missing"),
        StoredOutcome::Resolved(go("example.com/app/server.Missing")),
    );
    assert!(calls(
        &db,
        "example.com/app/server.Call",
        "example.com/app/server.Missing",
    ));
    assert_matches_cold(root, &db);
}

#[test]
fn removing_it_again_returns_the_caller_to_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(root, "server/caller.go", CALLER);
    write(root, "server/missing.go", MISSING);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");
    assert!(calls(
        &db,
        "example.com/app/server.Call",
        "example.com/app/server.Missing",
    ));

    fs::remove_file(root.join("server/missing.go")).unwrap();
    scan_go(root, &db).expect("second scan");

    assert_eq!(
        outcome(&db, "server/caller.go", "Missing"),
        unresolved(UnresolvedReason::NoMatchingDefinition),
        "the definition is gone, so the edge must be too",
    );
    assert!(!calls(
        &db,
        "example.com/app/server.Call",
        "example.com/app/server.Missing",
    ));
    assert_matches_cold(root, &db);
}

#[test]
fn a_higher_priority_definition_repoints_a_resolved_caller() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    // `Parse` resolves through the dot-import — the *second* candidate. The
    // miss on the first is indexed too, which is the only reason declaring
    // it later can reach this file.
    write(
        root,
        "server/dotter.go",
        "package server\n\nimport . \"example.com/app/util\"\n\nfunc Dot() {\n\tParse(\"x\")\n}\n",
    );
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");
    assert_eq!(
        outcome(&db, "server/dotter.go", "Parse"),
        StoredOutcome::Resolved(go("example.com/app/util.Parse")),
    );

    write(
        root,
        "server/parse.go",
        "package server\n\nfunc Parse(s string) string { return s }\n",
    );
    scan_go(root, &db).expect("second scan");

    assert_eq!(
        outcome(&db, "server/dotter.go", "Parse"),
        StoredOutcome::Resolved(go("example.com/app/server.Parse")),
        "the same package outranks a dot-import",
    );
    assert!(calls(
        &db,
        "example.com/app/server.Dot",
        "example.com/app/server.Parse",
    ));
    assert!(
        !calls(
            &db,
            "example.com/app/server.Dot",
            "example.com/app/util.Parse",
        ),
        "the edge re-points; it does not accumulate",
    );
    assert_matches_cold(root, &db);
}

#[test]
fn deleting_the_only_definition_of_a_package_removes_its_node_and_edges() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");
    assert_eq!(
        outcome(&db, "server/server.go", "util.Parse"),
        StoredOutcome::Resolved(go("example.com/app/util.Parse")),
    );

    // The importer is never edited: both its import and its qualified call
    // have to be woken by the deletion alone.
    fs::remove_file(root.join("util/util.go")).unwrap();
    scan_go(root, &db).expect("second scan");

    assert_eq!(
        outcome(&db, "server/server.go", "util.Parse"),
        unresolved(UnresolvedReason::NoMatchingDefinition),
    );
    assert_eq!(
        outcome(&db, "server/server.go", "example.com/app/util"),
        unresolved(UnresolvedReason::NoMatchingDefinition),
        "the package node went with its last file",
    );
    let snapshot = Store::open(&db).expect("reopen").snapshot().unwrap();
    assert!(!snapshot.nodes.contains_key(&go("example.com/app/util")));
    assert!(
        !snapshot
            .nodes
            .contains_key(&go("example.com/app/util.Parse"))
    );
    assert!(
        !snapshot
            .edges
            .iter()
            .any(|(_, dst, _)| *dst == go("example.com/app/util")
                || *dst == go("example.com/app/util.Parse")),
        "no edge is left pointing at a node nothing declares",
    );
    assert_matches_cold(root, &db);
}

#[test]
fn an_unrelated_edit_wakes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(root, "server/caller.go", CALLER);
    write(root, "server/missing.go", MISSING);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");
    let before = Store::open(&db).unwrap().snapshot().unwrap();

    // A trailing comment: the file's hash moves, no line above it does, and
    // it declares and removes nothing. Nothing outside it can be affected,
    // so nothing outside it may be rewritten.
    write(
        root,
        "server/server.go",
        &format!("{SERVER}\n// unrelated\n"),
    );
    scan_go(root, &db).expect("second scan");
    let after = Store::open(&db).unwrap().snapshot().unwrap();

    assert_eq!(before.nodes, after.nodes);
    assert_eq!(before.rows, after.rows);
    assert_eq!(before.edges, after.edges);
    assert_eq!(before.candidates, after.candidates);
    assert_ne!(
        before.files, after.files,
        "the edited file's hash is the one thing that moved",
    );
    assert_matches_cold(root, &db);
}

#[test]
fn the_candidate_index_names_only_the_files_that_probed_an_identity() {
    // The bound on the affected set, asserted where it is decided. Without
    // this, "wake the files that probed it" and "wake every file" are
    // indistinguishable on a fixture where they coincide.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(
        root,
        "server/a.go",
        "package server\n\nfunc A() {\n\tMissing()\n}\n",
    );
    write(
        root,
        "server/b.go",
        "package server\n\nfunc B() {\n\tMissing()\n}\n",
    );
    write(
        root,
        "server/c.go",
        "package server\n\nfunc C() {\n\thelper()\n}\n",
    );
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("scan");

    let rows = Store::open(&db)
        .unwrap()
        .rows_for(&[go("example.com/app/server.Missing")])
        .unwrap();
    let files: BTreeSet<&str> = rows.iter().map(|key| key.file.as_str()).collect();
    assert_eq!(
        files,
        BTreeSet::from(["server/a.go", "server/b.go"]),
        "only the files that probed the identity, not every file in the package",
    );
}

#[test]
fn an_affected_file_that_also_changed_is_resolved_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(root, "server/caller.go", CALLER);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");

    // One event that both declares the identity and edits the file that
    // probed it. The caller is in the changed set *and* named by the index,
    // so the two selections overlap and the event must resolve it once.
    write(root, "server/missing.go", MISSING);
    write(root, "server/caller.go", &format!("// edited\n{CALLER}"));
    scan_go(root, &db).expect("second scan");

    let rows = Store::open(&db).unwrap().snapshot().unwrap().rows;
    let caller: Vec<_> = rows
        .iter()
        .filter(|(key, _)| key.file == "server/caller.go" && key.raw_target == "Missing")
        .collect();
    assert_eq!(caller.len(), 1, "one row, not one per pass: {caller:?}");
    assert_eq!(caller[0].1.count, 1, "one occurrence, counted once");
    assert_matches_cold(root, &db);
}

#[test]
fn cold_equals_warm_survives_every_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(root, "server/caller.go", CALLER);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("initial scan");

    // Add the definition the caller wants.
    write(root, "server/missing.go", MISSING);
    scan_go(root, &db).expect("add");

    // Rename it: one identity added and one removed in a single event, both
    // of which the caller probed.
    write(
        root,
        "server/missing.go",
        "package server\n\nfunc Renamed() {}\n",
    );
    scan_go(root, &db).expect("edit");

    // Take the whole file away.
    fs::remove_file(root.join("server/missing.go")).unwrap();
    scan_go(root, &db).expect("delete");

    // And put it back.
    write(root, "server/missing.go", MISSING);
    scan_go(root, &db).expect("add back");

    assert_eq!(
        outcome(&db, "server/caller.go", "Missing"),
        StoredOutcome::Resolved(go("example.com/app/server.Missing")),
    );
    assert_matches_cold(root, &db);
}

#[test]
fn inserting_a_declaration_above_another_changes_no_unrelated_node_id() {
    // The executable form of FQN grammar invariant 3: no identity may carry
    // an occurrence-order component. If one did, everything below an insert
    // would re-key, every stored edge would dangle, and an incremental scan
    // would quietly rebuild half the graph.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("first scan");
    let before: BTreeSet<NodeId> = Store::open(&db)
        .unwrap()
        .snapshot()
        .unwrap()
        .nodes
        .into_keys()
        .collect();

    write(
        root,
        "util/util.go",
        "package util\n\nfunc Added() {}\n\nfunc Parse(s string) string { return s }\n",
    );
    scan_go(root, &db).expect("second scan");
    let after: BTreeSet<NodeId> = Store::open(&db)
        .unwrap()
        .snapshot()
        .unwrap()
        .nodes
        .into_keys()
        .collect();

    let lost: Vec<&NodeId> = before.difference(&after).collect();
    assert!(
        lost.is_empty(),
        "identities an unrelated insert moved: {lost:?}"
    );
    assert!(after.contains(&go("example.com/app/util.Added")));
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
