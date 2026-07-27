//! End-to-end: synthetic two-package module with exactly known counts.

use std::fs;

use arthron::UnresolvedReason;
use arthron::model::{Lang, NodeId, RefKind, node_id, reason_code};
use arthron::pipeline::scan;
use arthron::store::{NodeRecord, Store};

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// The node id a Go FQN would have, for identity assertions.
fn go(fqn: &str) -> NodeId {
    node_id(Lang::Go, fqn)
}

/// The stored record for a Go FQN, or `None` when nothing was stored.
fn node(store: &Store, fqn: &str) -> Option<NodeRecord> {
    store.node(&go(fqn)).expect("node lookup")
}

/// Whether a call edge runs from one Go FQN to another.
fn calls(store: &Store, src: &str, dst: &str) -> bool {
    store
        .has_edge(&go(src), &go(dst), RefKind::Call.code())
        .expect("edge lookup")
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
            "\thelper()\n",                       // → Resolved (same package)
            "\tmissing()\n",                      // → NoMatchingDefinition
            "\tconn.Close()\n",                   // → NeedsTypeInference
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
fn every_extracted_reference_has_exactly_one_stored_outcome() {
    // The non-negotiable, counted rather than asserted in prose: the three
    // outcome columns are a partition of the references the extractor found,
    // so they must sum to the reference count exactly. Over-counting is as
    // much a failure as dropping — one reference, one outcome.
    //
    // Hand-counted for `fixture`:
    //   util/util.go      0 imports, 0 calls
    //   server/server.go  2 imports ("fmt", "example.com/app/util")
    //                     5 calls   (fmt.Println, util.Parse, helper,
    //                                missing, conn.Close)
    const EXPECTED_REFERENCES: u64 = 7;

    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let report = scan(dir.path(), &dir.path().join("graph.redb")).expect("scan succeeds");
    let go = &report.per_lang[&Lang::Go.code()];

    assert_eq!(
        go.resolved + go.external + go.unresolved_total(),
        EXPECTED_REFERENCES,
        "resolved {} + external {} + unresolved {} must account for every \
         reference in the fixture, once each",
        go.resolved,
        go.external,
        go.unresolved_total(),
    );
}

#[test]
fn an_unaliased_internal_import_binds_the_declared_package_name() {
    // Go binds an unaliased import to the imported package's *declared*
    // name, which need not match its directory. Directory `utilx` declares
    // `package util`, so `util.Parse` is what the importing file writes —
    // and the qualifier `util` must be read as that import, not as a
    // variable of unknown type.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "utilx/util.go",
        "package util\n\nfunc Parse(s string) string { return s }\n",
    );
    write(
        root,
        "server/server.go",
        concat!(
            "package server\n\n",
            "import \"example.com/app/utilx\"\n\n",
            "func Serve(s string) string {\n",
            "\treturn util.Parse(s)\n",
            "}\n",
        ),
    );
    let db = root.join("graph.redb");

    let report = scan(root, &db).expect("scan succeeds");
    let tally = &report.per_lang[&Lang::Go.code()];
    // The import and the call, both resolved; nothing left over.
    assert_eq!(tally.resolved, 2);
    assert_eq!(tally.unresolved_total(), 0, "{:?}", tally.unresolved);

    // The import path stays the directory — only the binding name comes
    // from the declaration.
    let store = Store::open(&db).expect("reopen");
    assert!(calls(
        &store,
        "example.com/app/server.Serve",
        "example.com/app/utilx.Parse",
    ));
}

#[test]
fn init_is_not_a_node_and_its_calls_belong_to_the_package() {
    // `func init()` may be declared any number of times in a package and no
    // reference can name it. A node is a thing a reference can name, so it
    // is not one — and the calls inside it hang off the package, exactly
    // like a package-level variable initialiser.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "boot/a.go",
        "package boot\n\nfunc init() {\n\tsetup()\n}\n\nfunc setup() {}\n",
    );
    write(
        root,
        "boot/b.go",
        "package boot\n\nfunc init() {\n\tsetup()\n}\n",
    );
    let db = root.join("graph.redb");

    let report = scan(root, &db).expect("scan succeeds");
    let tally = &report.per_lang[&Lang::Go.code()];
    // Both `setup()` calls resolve — one row per file.
    assert_eq!(tally.resolved, 2);
    assert_eq!(tally.unresolved_total(), 0, "{:?}", tally.unresolved);

    let store = Store::open(&db).expect("reopen");
    assert!(
        node(&store, "example.com/app/boot.init").is_none(),
        "init is not nameable, so it must not be a node"
    );
    assert!(node(&store, "example.com/app/boot.setup").is_some());
    assert!(
        calls(&store, "example.com/app/boot", "example.com/app/boot.setup"),
        "a call inside init is sourced at the package node"
    );
}

#[test]
fn an_external_test_package_gets_its_own_namespace() {
    // `package graph_test` in directory `graph` is a different package that
    // happens to share a directory. Its definitions must not land in the
    // production package's namespace, where a same-package candidate from a
    // production file could wrongly hit one.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(root, "graph/graph.go", "package graph\n\nfunc Build() {}\n");
    write(
        root,
        "graph/graph_ext_test.go",
        concat!(
            "package graph_test\n\n",
            "import \"example.com/app/graph\"\n\n",
            "func helperT() {}\n\n",
            "func Run() {\n",
            "\thelperT()\n",
            "\tgraph.Build()\n",
            "}\n",
        ),
    );
    let db = root.join("graph.redb");

    let report = scan(root, &db).expect("scan succeeds");
    let tally = &report.per_lang[&Lang::Go.code()];
    // helperT(), graph.Build(), and the import of the production package.
    assert_eq!(tally.resolved, 3);
    assert_eq!(tally.unresolved_total(), 0, "{:?}", tally.unresolved);

    let store = Store::open(&db).expect("reopen");
    assert!(node(&store, "example.com/app/graph_test.helperT").is_some());
    assert!(
        node(&store, "example.com/app/graph.helperT").is_none(),
        "a test-file definition must not land in the production namespace"
    );
    assert!(
        node(&store, "example.com/app/graph_test.Run").is_some(),
        "every definition in the test file is namespaced, not just some"
    );
    // The production definition keeps its own FQN, and the test package
    // reaches it the ordinary way: through its import.
    assert!(node(&store, "example.com/app/graph.Build").is_some());
    assert!(calls(
        &store,
        "example.com/app/graph_test.Run",
        "example.com/app/graph.Build",
    ));
    assert!(calls(
        &store,
        "example.com/app/graph_test.Run",
        "example.com/app/graph_test.helperT",
    ));
}

#[test]
fn a_declared_package_name_survives_a_scan_that_does_not_touch_that_package() {
    // The warm path: only the importing file changed, so the declaring file
    // is not in the changed set and its `package util` clause is not read
    // this time. The name has to come out of the store, or the binding
    // would silently fall back to the directory name and the call would go
    // from resolved to unresolved on an edit that changed nothing about it.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "utilx/util.go",
        "package util\n\nfunc Parse(s string) string { return s }\n",
    );
    let server = |body: &str| {
        format!(
            concat!(
                "package server\n\n",
                "import \"example.com/app/utilx\"\n\n",
                "func Serve(s string) string {{\n",
                "{}",
                "}}\n",
            ),
            body
        )
    };
    write(
        root,
        "server/server.go",
        &server("\treturn util.Parse(s)\n"),
    );
    let db = root.join("graph.redb");

    let first = scan(root, &db).expect("first scan");
    assert_eq!(first.per_lang[&Lang::Go.code()].resolved, 2);

    // Edit only the importing file. `utilx/util.go` keeps its hash, so it
    // is not extracted again.
    write(
        root,
        "server/server.go",
        &server("\t_ = s\n\treturn util.Parse(s)\n"),
    );
    let second = scan(root, &db).expect("second scan");
    let tally = &second.per_lang[&Lang::Go.code()];
    assert_eq!(tally.resolved, 2);
    assert_eq!(tally.unresolved_total(), 0, "{:?}", tally.unresolved);
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
