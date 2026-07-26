//! End-to-end: synthetic two-package module with exactly known counts.

use std::fs;

use arthron::model::{Lang, reason_code};
use arthron::pipeline::scan;
use arthron::UnresolvedReason;

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn fixture(root: &std::path::Path) {
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
            "import (\n\t\"fmt\"\n\t\"example.com/app/util\"\n)\n\n",
            "func Serve(conn Conn) {\n",
            "\tfmt.Println(util.Parse(\"x\"))\n", // fmt → External, util.Parse → Resolved
            "\thelper()\n",                        // → Resolved (same package)
            "\tmissing()\n",                       // → NoMatchingDefinition
            "\tconn.Close()\n",                    // → NeedsTypeInference
            "}\n\n",
            "func helper() {}\n\n",
            "type Conn struct{}\n",
        ),
    );
}

#[test]
fn scan_reports_honest_per_language_counts() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let db = dir.path().join("graph.redb");

    let report = scan(dir.path(), &db).expect("scan succeeds");
    let go = &report.per_lang[&Lang::Go.code()];

    // Calls: util.Parse + helper resolved. Imports: example.com/app/util
    // resolved. fmt import + fmt.Println external. missing() unresolved
    // (NoMatchingDefinition), conn.Close() unresolved (NeedsTypeInference).
    assert_eq!(go.resolved, 3);
    assert_eq!(go.external, 2);
    assert_eq!(
        go.unresolved[&reason_code(&UnresolvedReason::NoMatchingDefinition)],
        1
    );
    assert_eq!(
        go.unresolved[&reason_code(&UnresolvedReason::NeedsTypeInference)],
        1
    );
    let rate = arthron::resolution_rate(go.resolved, go.unresolved_total()).unwrap();
    assert!((rate - 0.6).abs() < 1e-9);
}

#[test]
fn second_scan_of_unchanged_tree_reports_the_same() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let db = dir.path().join("graph.redb");
    let first = scan(dir.path(), &db).expect("first scan");
    // Warm path: every file hash matches, the changed set is empty, and
    // the report must come from the store, unchanged.
    let second = scan(dir.path(), &db).expect("second scan");
    assert_eq!(first.per_lang, second.per_lang);
}
