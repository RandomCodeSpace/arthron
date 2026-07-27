//! The incremental oracle for PHP: after any file event, the store must hold
//! exactly what a cold scan of the same tree would have built.
//!
//! `tests/incremental.rs` asserts the same thing for Go, and it is the same
//! machinery underneath — but the *inputs* a resolution reads are per
//! language, and PHP reads one Go does not. Rule 4 asks whether the walk
//! found the file PSR-4 maps a name onto, which makes the file set a phase-0
//! input the store has to invalidate on. A warm scan that skipped that
//! answered `ModuleNotFound` for a name a cold scan of the same tree called
//! `NoMatchingDefinition`, and no tally moved — both are unresolved — so only
//! a snapshot comparison could see it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::fs;
use std::path::Path;

use arthron::UnresolvedReason;
use arthron::model::reason_code;
use arthron::store::{Store, StoredOutcome};
use arthron::track_php::resolve::scan_php;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("a parent directory")).expect("create the directory");
    fs::write(path, content).expect("write the file");
}

/// The single stored outcome for one reference site.
fn outcome(db: &Path, file: &str, raw_target: &str) -> StoredOutcome {
    let rows = Store::open(db)
        .expect("open")
        .snapshot()
        .expect("snapshot")
        .rows;
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
    let cold_dir = tempfile::tempdir().expect("a scratch directory");
    let cold_db = cold_dir.path().join("cold.redb");
    let cold_report = scan_php(root, &cold_db).expect("cold scan");
    let cold = Store::open(&cold_db)
        .expect("open cold")
        .snapshot()
        .expect("cold snapshot");

    let warm_store = Store::open(warm_db).expect("open warm");
    let warm = warm_store.snapshot().expect("warm snapshot");
    let warm_report = warm_store.report().expect("warm report");

    same_map("files", &cold.files, &warm.files);
    same_map("nodes", &cold.nodes, &warm.nodes);
    same_map("rows", &cold.rows, &warm.rows);
    same_set("edges", &cold.edges, &warm.edges);
    same_map("candidates", &cold.candidates, &warm.candidates);
    assert_eq!(
        cold, warm,
        "the snapshots differ beyond the fields compared"
    );
    assert_eq!(cold_report, warm_report, "the reports differ");
}

/// A repository whose `composer.json` claims `App\` for `src/`.
fn fixture(root: &Path) {
    write(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"App\\":"src/"}}}"#,
    );
    write(
        root,
        "src/Client.php",
        "<?php\nnamespace App;\n\nuse App\\Cookie\\Jar;\nuse App\\Missing\\Thing;\n\nclass Client {}\n",
    );
    write(
        root,
        "src/Cookie/Jar.php",
        "<?php\nnamespace App\\Cookie;\n\nclass Jar {}\n",
    );
}

#[test]
fn cold_equals_warm_on_an_unchanged_tree() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_php(root, &db).expect("cold scan");
    scan_php(root, &db).expect("warm scan");
    assert_matches_cold(root, &db);
}

#[test]
fn editing_a_file_lands_the_same_state_as_a_cold_scan() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_php(root, &db).expect("cold scan");
    // The file set is unchanged, so this event stays incremental: what the
    // config digest fences on is the *set of paths*, and a file's contents
    // are carried by its own hash.
    write(
        root,
        "src/Cookie/Jar.php",
        "<?php\nnamespace App\\Cookie;\n\nclass Jar {\n    public function clear(): void {}\n}\n",
    );
    scan_php(root, &db).expect("warm scan");
    assert_matches_cold(root, &db);
}

#[test]
fn adding_a_definition_repoints_an_unresolved_import_in_an_unchanged_file() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_php(root, &db).expect("cold scan");
    assert_eq!(
        outcome(&db, "src/Client.php", "App\\Missing\\Thing"),
        unresolved(UnresolvedReason::ModuleNotFound),
    );

    // `src/Client.php` is not edited. The identity its import probed starts
    // being declared, and the candidate index is what wakes the row.
    write(
        root,
        "src/Missing/Thing.php",
        "<?php\nnamespace App\\Missing;\n\nclass Thing {}\n",
    );
    scan_php(root, &db).expect("warm scan");
    assert!(
        matches!(
            outcome(&db, "src/Client.php", "App\\Missing\\Thing"),
            StoredOutcome::Resolved(_)
        ),
        "the import did not repoint",
    );
    assert_matches_cold(root, &db);
}

#[test]
fn adding_a_file_that_declares_a_different_name_still_moves_the_reason() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_php(root, &db).expect("cold scan");
    assert_eq!(
        outcome(&db, "src/Client.php", "App\\Missing\\Thing"),
        unresolved(UnresolvedReason::ModuleNotFound),
    );

    // The file PSR-4 maps `App\Missing\Thing` onto now exists and declares
    // something else. No identity the row probed moved — `App\Missing#Thing`
    // is still undeclared — so the candidate index wakes nothing, and only
    // the file set says the answer changed: the map no longer points at
    // nothing, so the miss is arthron's own rather than composer's.
    //
    // Both reasons are `Unresolved`, so every tally is identical either way.
    // This is the assertion that a report cannot make.
    write(
        root,
        "src/Missing/Thing.php",
        "<?php\nnamespace App\\Missing;\n\nclass Other {}\n",
    );
    scan_php(root, &db).expect("warm scan");
    assert_eq!(
        outcome(&db, "src/Client.php", "App\\Missing\\Thing"),
        unresolved(UnresolvedReason::NoMatchingDefinition),
        "the warm store kept a verdict a cold scan of this tree no longer gives",
    );
    assert_matches_cold(root, &db);
}

#[test]
fn deleting_that_file_again_returns_the_reason_to_module_not_found() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    write(
        root,
        "src/Missing/Thing.php",
        "<?php\nnamespace App\\Missing;\n\nclass Other {}\n",
    );
    scan_php(root, &db).expect("cold scan");
    assert_eq!(
        outcome(&db, "src/Client.php", "App\\Missing\\Thing"),
        unresolved(UnresolvedReason::NoMatchingDefinition),
    );

    fs::remove_file(root.join("src/Missing/Thing.php")).expect("delete the file");
    scan_php(root, &db).expect("warm scan");
    assert_eq!(
        outcome(&db, "src/Client.php", "App\\Missing\\Thing"),
        unresolved(UnresolvedReason::ModuleNotFound),
    );
    assert_matches_cold(root, &db);
}

#[test]
fn rewriting_the_manifest_lands_a_cold_scans_store() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    scan_php(root, &db).expect("cold scan");

    // The prefix no longer claims `App\`, so every name under it leaves rule
    // 4 for rule 5 and becomes external. Nothing about any file changed.
    write(
        root,
        "composer.json",
        r#"{"autoload":{"psr-4":{"Other\\":"src/"}}}"#,
    );
    scan_php(root, &db).expect("warm scan");
    assert_eq!(
        outcome(&db, "src/Client.php", "App\\Missing\\Thing"),
        StoredOutcome::External("App".to_string()),
    );
    assert_matches_cold(root, &db);
}
