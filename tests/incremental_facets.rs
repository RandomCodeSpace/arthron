//! The incremental oracle for stored facets: an edit that changes only what
//! a declaration *is* has to reach the references that branched on it.
//!
//! `tests/incremental.rs` covers the definition phase and
//! `tests/incremental_supers.rs` the supertype phase. This file is the half
//! that only exists once a facet is readable: turning `class T` into
//! `interface T` moves no identity — `T`'s FQN is unchanged — and until
//! stage 3 it moved no payload either, so a file that read the kind and the
//! facets saw one of them change and was never told.
//!
//! The tree is chosen so that the facet is the *only* thing that moves in the
//! reading file's candidate set: the creation site passes one argument, so it
//! probes `<init>/1` and the varargs keys and never `<init>/0` — the implicit
//! constructor node that appears and disappears with the class. Drop the
//! facets out of `NodePayload` and this test fails; drop them out of
//! `NodeRecord` alone and it still fails, because the payload is derived from
//! the record.

use std::fs;
use std::path::Path;

use arthron::model::{RefKind, reason_name};
use arthron::store::{Snapshot, Store, StoredOutcome};
use arthron::track_java::scan_java;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn snapshot(db: &Path) -> Snapshot {
    Store::open(db).expect("open").snapshot().unwrap()
}

/// Scan `root` cold into a throwaway store and compare it, whole, with what
/// the incremental scans left in `warm_db`.
#[track_caller]
fn assert_matches_cold(root: &Path, warm_db: &Path) {
    let cold_dir = tempfile::tempdir().unwrap();
    let cold_db = cold_dir.path().join("cold.redb");
    let cold_report = scan_java(root, &cold_db).expect("cold scan");
    let cold = Store::open(&cold_db)
        .expect("open cold")
        .snapshot()
        .unwrap();

    let warm_store = Store::open(warm_db).expect("open warm");
    let warm = warm_store.snapshot().unwrap();
    let warm_report = warm_store.report().unwrap();

    assert_eq!(cold.nodes, warm.nodes, "the nodes differ");
    assert_eq!(cold.rows, warm.rows, "the reference rows differ");
    assert_eq!(cold.edges, warm.edges, "the edges differ");
    assert_eq!(
        cold.candidates, warm.candidates,
        "the candidate index differs"
    );
    assert_eq!(
        cold, warm,
        "the snapshots differ beyond the fields compared"
    );
    assert_eq!(cold_report, warm_report);
}

/// The single stored outcome of one reference site, rendered.
///
/// Keyed by kind as well as by text: `new Target(1){…}` is both a creation
/// site and, through the anonymous class it declares, a supertype named by
/// that class — two rows with the same raw target, and only one of them is
/// what C-05 decides.
#[track_caller]
fn outcome(db: &Path, file: &str, raw_target: &str, kind: RefKind) -> String {
    let rows = snapshot(db).rows;
    let mut found: Vec<String> = rows
        .iter()
        .filter(|(key, _)| {
            key.file == file && key.raw_target == raw_target && key.kind == kind.code()
        })
        .map(|(_, row)| match &row.outcome {
            StoredOutcome::Resolved(id) => format!("RESOLVED {id:?}"),
            StoredOutcome::External(package) => format!("EXTERNAL {package}"),
            StoredOutcome::Unresolved(code) => reason_name(*code).to_string(),
        })
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one `{raw_target}` row in {file}, found {found:?}",
    );
    found.pop().expect("one row")
}

const USE: &str = r#"package com.acme;
public class Use {
    Object o = new Target(1) {
        public String tag() { return "t"; }
    };
}
"#;

#[test]
fn turning_a_class_into_an_interface_repoints_a_creation_in_an_unchanged_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "com/acme/Target.java",
        "package com.acme;\npublic class Target {\n    public Target() { }\n}\n",
    );
    write(root, "com/acme/Use.java", USE);
    let db = root.join("graph.redb");
    scan_java(root, &db).expect("first scan");
    assert_eq!(
        outcome(&db, "com/acme/Use.java", "Target", RefKind::New),
        "UnindexedSupertype",
        "a class with no one-argument constructor is a miss, not `java.lang`",
    );

    // One file rewritten, and the file that reads it untouched. `Target`'s
    // identity does not move and neither does any member key `Use.java`
    // probed — only what the declaration *is*.
    write(
        root,
        "com/acme/Target.java",
        "package com.acme;\npublic interface Target {\n    String tag();\n}\n",
    );
    scan_java(root, &db).expect("second scan");
    assert_eq!(
        outcome(&db, "com/acme/Use.java", "Target", RefKind::New),
        "EXTERNAL jdk:java.lang",
        "§15.9.5.1: the anonymous class implements the interface and \
         extends `Object`, whose constructor it invokes",
    );
    assert_matches_cold(root, &db);
}

#[test]
fn turning_an_interface_back_into_a_class_wakes_the_same_reference() {
    // The other direction, because a widening that only fired on losing a
    // facet would pass the test above and still strand every reference that
    // gained one.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "com/acme/Target.java",
        "package com.acme;\npublic interface Target {\n    String tag();\n}\n",
    );
    write(root, "com/acme/Use.java", USE);
    let db = root.join("graph.redb");
    scan_java(root, &db).expect("first scan");
    assert_eq!(
        outcome(&db, "com/acme/Use.java", "Target", RefKind::New),
        "EXTERNAL jdk:java.lang",
    );

    write(
        root,
        "com/acme/Target.java",
        "package com.acme;\npublic class Target {\n    public Target() { }\n}\n",
    );
    scan_java(root, &db).expect("second scan");
    assert_eq!(
        outcome(&db, "com/acme/Use.java", "Target", RefKind::New),
        "UnindexedSupertype",
    );
    assert_matches_cold(root, &db);
}
