//! The query surface, end to end: scan a fixture tree, then ask the stored
//! graph about names in it.
//!
//! Every assertion is against a tree whose every edge is written out below,
//! so an expected count is a fact about the fixture rather than a number
//! copied out of a previous run.

use std::fs;
use std::path::Path;

use arthron::model::{DefKind, Domain, NodeId, RefKind, node_id};
use arthron::pipeline::scan_repo;
use arthron::query::{DEFAULT_IMPACT_DEPTH, NameIndex, NodeKind, definition, impact, references};
use arthron::store::{ReadStore, StoredOutcome};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// The node id a Go FQN would have.
fn go(fqn: &str) -> NodeId {
    node_id(Domain::Go, fqn)
}

/// A three-package module. `Parse` is called from two enclosers in one file;
/// `Serve` is called from a third package, so the reverse closure of `Parse`
/// is two layers deep.
///
/// ```text
///   api#Handle ──▶ server#Serve ──▶ util#Parse
///                  server#helper ─▶ util#Parse
/// ```
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

/// Scan a fixture tree and hand back a read-only handle on its graph.
///
/// The writing [`arthron::store::Store`] is dropped inside `scan_repo`, so
/// the read-only open that follows is not contending with it.
fn scanned(dir: &Path) -> ReadStore {
    fixture(dir);
    let db = dir.join("graph.redb");
    scan_repo(dir, &db).expect("the fixture scans");
    ReadStore::open(&db).expect("the store opens read-only")
}

#[test]
fn a_full_fqn_selects_exactly_its_node() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());
    let index = NameIndex::build(&store).expect("index builds");

    let hits = index.lookup("example.com/app/util#Parse");
    assert_eq!(hits.len(), 1, "one FQN, one node: {hits:?}");
    assert_eq!(hits[0].id, go("example.com/app/util#Parse"));
    assert_eq!(hits[0].kind, NodeKind::Definition(DefKind::Function));
}

#[test]
fn a_bare_suffix_selects_every_node_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());
    let index = NameIndex::build(&store).expect("index builds");

    let hits = index.lookup("Parse");
    assert_eq!(hits.len(), 1, "only one definition ends in Parse: {hits:?}");
    assert_eq!(hits[0].name, "example.com/app/util#Parse");
}

#[test]
fn a_suffix_only_matches_at_a_separator() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());
    let index = NameIndex::build(&store).expect("index builds");

    // `arse` ends `Parse`, but not at a boundary: a suffix that starts
    // mid-identifier names nothing, and answering it would be a guess.
    assert!(index.lookup("arse").is_empty());
}

#[test]
fn ambiguity_is_an_answer_and_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    // A second package declaring the same unqualified name. Two FQNs, two
    // identities, one bare suffix.
    write(
        dir.path(),
        "text/text.go",
        "package text\n\nfunc Parse(s string) string { return s }\n",
    );
    let db = dir.path().join("graph.redb");
    scan_repo(dir.path(), &db).expect("the fixture scans");
    let store = ReadStore::open(&db).expect("the store opens read-only");
    let index = NameIndex::build(&store).expect("index builds");

    let mut names: Vec<String> = index.lookup("Parse").into_iter().map(|m| m.name).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "example.com/app/text#Parse".to_string(),
            "example.com/app/util#Parse".to_string(),
        ],
    );

    // And the exact spelling of one of them still selects only that one: an
    // exact FQN is never widened into its own suffix search.
    let exact = index.lookup("example.com/app/util#Parse");
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].name, "example.com/app/util#Parse");
}

#[test]
fn a_missing_name_selects_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());
    let index = NameIndex::build(&store).expect("index builds");

    assert!(index.lookup("NoSuchThing").is_empty());
    assert!(index.lookup("").is_empty(), "the empty query names nothing");
}

#[test]
fn an_empty_store_answers_empty_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.redb");
    // Created and closed without a scan: every table exists and every table
    // is empty.
    drop(arthron::store::Store::open(&db).expect("the store is created"));

    let store = ReadStore::open(&db).expect("an unscanned store still opens");
    let index = NameIndex::build(&store).expect("index builds over no nodes");
    assert!(index.lookup("Parse").is_empty());

    let id = go("example.com/app/util#Parse");
    assert_eq!(definition(&store, &id).expect("def query"), None);
    assert!(references(&store, &id).expect("refs query").is_empty());
    assert!(
        impact(&store, &id, DEFAULT_IMPACT_DEPTH)
            .expect("impact query")
            .layers
            .is_empty()
    );
}

#[test]
fn definition_carries_the_record_and_every_declaration_site() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());

    let id = go("example.com/app/util#Parse");
    let def = definition(&store, &id).expect("def query").expect("a node");
    assert_eq!(def.node.name, "example.com/app/util#Parse");
    assert_eq!(def.node.kind, NodeKind::Definition(DefKind::Function));
    assert_eq!(def.declarations.len(), 1);
    assert_eq!(def.declarations[0].file, "util/util.go");
    assert_eq!(def.declarations[0].line, 3);
    assert!(
        def.targets.is_empty(),
        "an ordinary function aliases nothing"
    );
}

#[test]
fn definition_of_an_absent_identity_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());

    assert_eq!(
        definition(&store, &go("example.com/app/util#Nope")).expect("def query"),
        None,
    );
}

#[test]
fn references_lists_every_row_that_resolved_to_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());

    let id = go("example.com/app/util#Parse");
    let sites = references(&store, &id).expect("refs query");
    // `util.Parse` is called once in `Serve` and once in `helper`. The two
    // are separate rows because a row key carries its encloser.
    assert_eq!(sites.len(), 2, "{sites:?}");
    for site in &sites {
        assert_eq!(site.file, "server/server.go");
        assert_eq!(site.kind, Some(RefKind::Call));
        assert_eq!(site.raw_target, "util.Parse");
        assert_eq!(site.outcome, StoredOutcome::Resolved(id));
        assert_eq!(site.count, 1);
    }
    let mut enclosers: Vec<&str> = sites.iter().map(|s| s.enclosing.as_str()).collect();
    enclosers.sort_unstable();
    assert_eq!(
        enclosers,
        vec![
            "example.com/app/server#Serve",
            "example.com/app/server#helper"
        ],
    );
    // Sites come out ordered, so a caller printing them prints the same list
    // twice running.
    let mut lines: Vec<u32> = sites.iter().map(|s| s.line).collect();
    lines.sort_unstable();
    // `server/server.go` line 6 is the call inside `Serve`, line 11 the one
    // inside `helper`.
    assert_eq!(lines, vec![6, 11]);
}

#[test]
fn references_to_an_uncalled_node_is_empty_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());

    let id = go("example.com/app/api#Handle");
    assert!(references(&store, &id).expect("refs query").is_empty());
}

#[test]
fn impact_is_the_layered_reverse_closure() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());

    let id = go("example.com/app/util#Parse");
    let result = impact(&store, &id, 3).expect("impact query");
    assert_eq!(result.layers.len(), 2, "{result:?}");

    let mut first: Vec<&str> = result.layers[0].iter().map(|m| m.name.as_str()).collect();
    first.sort_unstable();
    assert_eq!(
        first,
        vec![
            "example.com/app/server#Serve",
            "example.com/app/server#helper"
        ],
    );

    let second: Vec<&str> = result.layers[1].iter().map(|m| m.name.as_str()).collect();
    assert_eq!(second, vec!["example.com/app/api#Handle"]);
    assert!(!result.truncated, "depth 3 covers a closure two hops deep");
}

#[test]
fn impact_stops_at_the_depth_bound_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());

    let id = go("example.com/app/util#Parse");
    let result = impact(&store, &id, 1).expect("impact query");
    assert_eq!(result.layers.len(), 1);
    assert!(
        result.truncated,
        "`api#Handle` sits one hop past the bound and must be declared cut",
    );

    // Depth zero walks nothing at all, and still reports that there was
    // something to walk.
    let none = impact(&store, &id, 0).expect("impact query");
    assert!(none.layers.is_empty());
    assert!(none.truncated);
}

#[test]
fn impact_terminates_on_a_cycle() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "go.mod", "module example.com/app\n\ngo 1.22\n");
    // Mutual recursion: `a` calls `b`, `b` calls `a`. A reverse walk from
    // either re-enters the other, and only the visited set ends it.
    write(
        dir.path(),
        "loop/loop.go",
        concat!(
            "package loop\n\n",
            "func a() {\n\tb()\n}\n\n",
            "func b() {\n\ta()\n}\n\n",
            "func Start() {\n\ta()\n}\n",
        ),
    );
    let db = dir.path().join("graph.redb");
    scan_repo(dir.path(), &db).expect("the fixture scans");
    let store = ReadStore::open(&db).expect("the store opens read-only");

    let result = impact(&store, &go("example.com/app/loop#a"), 10).expect("impact query");
    let reached: Vec<&str> = result
        .layers
        .iter()
        .flatten()
        .map(|m| m.name.as_str())
        .collect();
    // `b` and `Start` call `a`; `a` calls `b`, but `a` is the node asked
    // about and never re-enters the answer.
    assert_eq!(reached.len(), 2, "{result:?}");
    assert!(reached.contains(&"example.com/app/loop#b"));
    assert!(reached.contains(&"example.com/app/loop#Start"));
    assert!(!result.truncated, "the walk ran out of graph, not of depth");
}

#[test]
fn a_store_held_open_for_writing_fails_the_query_rather_than_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.redb");
    let writer = arthron::store::Store::open(&db).expect("the store is created");

    let err = ReadStore::open(&db).expect_err("a locked store must not open");
    assert!(
        err.contains("open for writing"),
        "the error has to say why: {err}",
    );
    drop(writer);

    // And the moment the writer is gone the same query works: the refusal is
    // a lock, not a corruption.
    ReadStore::open(&db).expect("the store opens once the writer is gone");
}

#[test]
fn a_store_that_does_not_exist_is_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let err =
        ReadStore::open(&dir.path().join("absent.redb")).expect_err("there is nothing to open");
    assert!(err.contains("absent.redb"), "{err}");
}
