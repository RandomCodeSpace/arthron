//! The incremental oracle for the supertype phase: an edit to what a type
//! *extends* has to reach the member references in files nobody touched.
//!
//! `tests/incremental.rs` asks the same question of the definition phase, over
//! Go, whose resolver declares no link kinds. This file is the half that only
//! exists once a phase derives a fact from two files at once: a type's
//! supertypes are not in its identity and not in its payload, so an edit that
//! rewrites an `extends` clause moves no node at all — and every member
//! lookup below that type answered a question it must now answer differently.
//!
//! Every test edits one file and asserts about another. A widening that
//! degenerated to "only the changed files" passes any test whose reader sits
//! in the file that changed, so none of them does.

use std::fs;
use std::path::Path;

use arthron::model::{Domain, node_id, reason_name};
use arthron::store::{Report, Snapshot, Store, StoredOutcome};
use arthron::track_java::scan_java;
use arthron::track_python::resolve::scan_python;

type Scan = fn(&Path, &Path) -> Result<Report, String>;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Scan `root` cold into a throwaway store and compare it, whole, with what
/// the incremental scans left in `warm_db`.
///
/// The comparison is the whole [`Snapshot`], which now carries the supertype
/// half: a warm store that kept a stale row for a file it did re-read, or
/// dropped one for a file it did not, differs here and nowhere in the tallies.
#[track_caller]
fn assert_matches_cold(scan: Scan, root: &Path, warm_db: &Path) {
    let cold_dir = tempfile::tempdir().unwrap();
    let cold_db = cold_dir.path().join("cold.redb");
    let cold_report = scan(root, &cold_db).expect("cold scan");
    let cold = Store::open(&cold_db)
        .expect("open cold")
        .snapshot()
        .unwrap();

    let warm_store = Store::open(warm_db).expect("open warm");
    let warm = warm_store.snapshot().unwrap();
    let warm_report = warm_store.report().unwrap();

    assert_eq!(cold.supers, warm.supers, "the supertype half differs");
    assert_eq!(cold.rows, warm.rows, "the reference rows differ");
    assert_eq!(cold.nodes, warm.nodes, "the nodes differ");
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

fn snapshot(db: &Path) -> Snapshot {
    Store::open(db).expect("open").snapshot().unwrap()
}

/// The single stored outcome of one reference site, rendered.
#[track_caller]
fn outcome(db: &Path, file: &str, raw_target: &str) -> String {
    let rows = snapshot(db).rows;
    let mut found: Vec<String> = rows
        .iter()
        .filter(|(key, _)| key.file == file && key.raw_target == raw_target)
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

/// `RESOLVED` plus the identity of a Java FQN, as [`outcome`] renders it.
fn resolved_java(fqn: &str) -> String {
    format!("RESOLVED {:?}", node_id(Domain::Jvm, fqn))
}

fn resolved_python(fqn: &str) -> String {
    format!("RESOLVED {:?}", node_id(Domain::Python, fqn))
}

// ---------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------

const TOP: &str = r#"package com.acme;
public class Top {
    public String top() { return "t"; }
}
"#;

const OTHER: &str = r#"package com.acme;
public class Other {
    public String other() { return "o"; }
}
"#;

// The receiver is a field. A field is a node, so `m.top()` stays inside both
// terms of the resolution rate and the supertype relation is what decides it;
// a parameter would be answered `LocalBinding` by the uniform root-binding
// rule on `arthron::UnresolvedReason::LocalBinding` before any hierarchy walk
// ran, and this file would then measure the policy rather than invalidation.
const USE: &str = r#"package com.acme;
public class Use {
    Mid m;
    void go() {
        m.top();
    }
}
"#;

fn java_tower(root: &Path) {
    write(root, "com/acme/Top.java", TOP);
    write(root, "com/acme/Other.java", OTHER);
    write(
        root,
        "com/acme/Mid.java",
        "package com.acme;\npublic class Mid extends Top {\n}\n",
    );
    write(root, "com/acme/Use.java", USE);
}

#[test]
fn rewriting_an_extends_clause_repoints_a_member_call_in_an_unchanged_file() {
    // The case the second widening exists for. `Mid`'s identity is its FQN
    // and its payload is its kind: rewriting `extends Top` to `extends Other`
    // moves neither, so the definition phase's comparison sees nothing at all
    // — and every member lookup that walked through `Mid` was answered by the
    // clause that just changed.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    java_tower(root);
    let db = root.join("graph.redb");
    scan_java(root, &db).expect("first scan");
    assert_eq!(
        outcome(&db, "com/acme/Use.java", "m.top"),
        resolved_java("com.acme#Top.top/0"),
        "the member is two files away and one hop up",
    );

    write(
        root,
        "com/acme/Mid.java",
        "package com.acme;\npublic class Mid extends Other {\n}\n",
    );
    scan_java(root, &db).expect("second scan");

    assert_eq!(
        outcome(&db, "com/acme/Use.java", "m.top"),
        "UnindexedSupertype",
        "`Top` is no longer above `Mid`, so the edge must be gone",
    );
    assert_matches_cold(scan_java, root, &db);
}

#[test]
fn declaring_a_missing_base_reaches_a_member_call_in_an_unchanged_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "com/acme/Mid.java",
        "package com.acme;\npublic class Mid extends Top {\n}\n",
    );
    write(root, "com/acme/Use.java", USE);
    let db = root.join("graph.redb");
    scan_java(root, &db).expect("first scan");
    assert_eq!(
        outcome(&db, "com/acme/Use.java", "m.top"),
        "UnindexedSupertype",
        "nothing declares `Top` yet",
    );

    // One new file. `Use.java` is not rewritten, and the identity it has to
    // be woken by is not one it ever named: it named `Mid`, whose *row*
    // changed because the base it states finally placed.
    write(root, "com/acme/Top.java", TOP);
    scan_java(root, &db).expect("second scan");

    assert_eq!(
        outcome(&db, "com/acme/Use.java", "m.top"),
        resolved_java("com.acme#Top.top/0"),
    );
    assert_matches_cold(scan_java, root, &db);
}

#[test]
fn deleting_a_base_class_lands_a_cold_scans_store() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    java_tower(root);
    let db = root.join("graph.redb");
    scan_java(root, &db).expect("first scan");

    fs::remove_file(root.join("com/acme/Top.java")).unwrap();
    scan_java(root, &db).expect("second scan");

    assert_eq!(
        outcome(&db, "com/acme/Use.java", "m.top"),
        "UnindexedSupertype",
    );
    assert_matches_cold(scan_java, root, &db);
}

#[test]
fn a_java_edit_that_touches_no_hierarchy_wakes_nothing() {
    // The bound, asserted where it is decided: the supertype widening must
    // select on a row that *moved*, not on every row the phase rewrote.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    java_tower(root);
    let db = root.join("graph.redb");
    scan_java(root, &db).expect("first scan");
    let before = snapshot(&db);

    write(root, "com/acme/Other.java", &format!("{OTHER}\n// tail\n"));
    scan_java(root, &db).expect("second scan");
    let after = snapshot(&db);

    assert_eq!(before.rows, after.rows);
    assert_eq!(before.edges, after.edges);
    assert_eq!(before.candidates, after.candidates);
    assert_eq!(before.supers, after.supers);
    assert_ne!(
        before.files, after.files,
        "only the edited file's hash moved"
    );
    assert_matches_cold(scan_java, root, &db);
}

// ---------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------

fn python_tower(root: &Path) {
    write(root, "app/__init__.py", "");
    write(
        root,
        "app/top.py",
        "class Top:\n    def top(self):\n        return 1\n",
    );
    write(
        root,
        "app/other.py",
        "class Other:\n    def other(self):\n        return 2\n",
    );
    write(
        root,
        "app/mid.py",
        "from .top import Top\n\n\nclass Mid(Top):\n    pass\n",
    );
    write(
        root,
        "app/use.py",
        concat!(
            "from .mid import Mid\n",
            "\n",
            "\n",
            "class Use(Mid):\n",
            "    def go(self):\n",
            "        return self.top()\n",
        ),
    );
}

#[test]
fn rewriting_a_base_class_repoints_a_self_call_in_an_unchanged_module() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    python_tower(root);
    let db = root.join("graph.redb");
    scan_python(root, &db).expect("first scan");
    assert_eq!(
        outcome(&db, "app/use.py", "self.top"),
        resolved_python("app.top#Top.top"),
    );

    write(
        root,
        "app/mid.py",
        "from .other import Other\n\n\nclass Mid(Other):\n    pass\n",
    );
    scan_python(root, &db).expect("second scan");

    assert_eq!(
        outcome(&db, "app/use.py", "self.top"),
        "NoMatchingDefinition",
        "the MRO is still fully readable, and `top` is no longer in it",
    );
    assert_matches_cold(scan_python, root, &db);
}

#[test]
fn declaring_a_missing_base_module_reaches_a_self_call_in_an_unchanged_module() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "app/__init__.py", "");
    write(
        root,
        "app/mid.py",
        "from .top import Top\n\n\nclass Mid(Top):\n    pass\n",
    );
    write(
        root,
        "app/use.py",
        concat!(
            "from .mid import Mid\n",
            "\n",
            "\n",
            "class Use(Mid):\n",
            "    def go(self):\n",
            "        return self.top()\n",
        ),
    );
    let db = root.join("graph.redb");
    scan_python(root, &db).expect("first scan");
    assert_eq!(
        outcome(&db, "app/use.py", "self.top"),
        "UnindexedSupertype",
        "`Mid`'s base is not in the graph, so the chain below it is short",
    );

    write(
        root,
        "app/top.py",
        "class Top:\n    def top(self):\n        return 1\n",
    );
    scan_python(root, &db).expect("second scan");

    assert_eq!(
        outcome(&db, "app/use.py", "self.top"),
        resolved_python("app.top#Top.top"),
    );
    assert_matches_cold(scan_python, root, &db);
}

#[test]
fn deleting_a_base_module_lands_a_cold_scans_store() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    python_tower(root);
    let db = root.join("graph.redb");
    scan_python(root, &db).expect("first scan");

    fs::remove_file(root.join("app/top.py")).unwrap();
    scan_python(root, &db).expect("second scan");

    assert_eq!(outcome(&db, "app/use.py", "self.top"), "UnindexedSupertype",);
    assert_matches_cold(scan_python, root, &db);
}

#[test]
fn a_python_hierarchy_survives_every_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    python_tower(root);
    let db = root.join("graph.redb");
    scan_python(root, &db).expect("initial scan");

    // Re-point the middle class, put it back, delete the top, restore it.
    write(
        root,
        "app/mid.py",
        "from .other import Other\n\n\nclass Mid(Other):\n    pass\n",
    );
    scan_python(root, &db).expect("re-point");
    write(
        root,
        "app/mid.py",
        "from .top import Top\n\n\nclass Mid(Top):\n    pass\n",
    );
    scan_python(root, &db).expect("restore");
    fs::remove_file(root.join("app/top.py")).unwrap();
    scan_python(root, &db).expect("delete");
    write(
        root,
        "app/top.py",
        "class Top:\n    def top(self):\n        return 1\n",
    );
    scan_python(root, &db).expect("add back");

    assert_eq!(
        outcome(&db, "app/use.py", "self.top"),
        resolved_python("app.top#Top.top"),
    );
    assert_matches_cold(scan_python, root, &db);
}
