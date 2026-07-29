//! End-to-end: synthetic two-package module with exactly known counts.

use std::fs;

use arthron::UnresolvedReason;
use arthron::model::{Domain, Lang, NodeId, RefKind, node_id, reason_code};
use arthron::pipeline::scan_go;
use arthron::store::{NodeRecord, Store};

/// Whether an edge of the given kind runs from one Go FQN to another.
fn links(store: &Store, src: &str, dst: &str, kind: RefKind) -> bool {
    store
        .has_edge(&go(src), &go(dst), kind.code())
        .expect("edge lookup")
}

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// The node id a Go FQN would have, for identity assertions.
fn go(fqn: &str) -> NodeId {
    node_id(Domain::Go, fqn)
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
            "var pool Conn\n\n",
            "func Serve(conn Conn) {\n",
            "\tfmt.Println(util.Parse(\"x\"))\n", // fmt → External, util.Parse → Resolved
            "\thelper()\n",                       // → Resolved (same package)
            "\tmissing()\n",                      // → NoMatchingDefinition
            "\tconn.Close()\n",                   // parameter → LocalBinding
            "\tpool.Close()\n",                   // package-level → NeedsTypeInference
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

    let report = scan_go(dir.path(), &db).expect("scan succeeds");
    let go = &report.per_lang[&Lang::Go.code()];

    // Calls: util.Parse + helper resolved. Imports: example.com/app/util
    // resolved. Type uses: `Conn` in `var pool Conn` and in `Serve`'s
    // signature, both resolved. fmt import + fmt.Println external, and
    // `Parse`'s two `string`s are Go universe names, so external too.
    // missing() unresolved (NoMatchingDefinition), pool.Close() unresolved
    // (NeedsTypeInference). conn.Close() names a parameter, so it is a local
    // binding — reported, and outside both terms of the rate.
    assert_eq!(go.resolved, 5);
    assert_eq!(go.external, 4);
    assert_eq!(go.local_binding, 1);
    assert_eq!(
        go.unresolved[&reason_code(&UnresolvedReason::NoMatchingDefinition)],
        1
    );
    assert_eq!(
        go.unresolved[&reason_code(&UnresolvedReason::NeedsTypeInference)],
        1
    );
    assert!(
        !go.unresolved
            .contains_key(&reason_code(&UnresolvedReason::LocalBinding)),
        "a local binding never enters the unresolved map",
    );
    let rate = arthron::resolution_rate(go.resolved, go.unresolved_total()).unwrap();
    assert!((rate - 5.0 / 7.0).abs() < 1e-9);
}

#[test]
fn current_extractors_freeze_argument_types_to_none() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let db = dir.path().join("graph.redb");

    scan_go(dir.path(), &db).expect("scan succeeds");
    let snapshot = Store::open(&db)
        .expect("store opens")
        .snapshot()
        .expect("store snapshots");
    assert!(
        !snapshot.rows.is_empty(),
        "the fixture must exercise row keys"
    );
    assert!(
        snapshot.rows.keys().all(|key| key.arg_types.is_none()),
        "C0 changes key representation only; language-owned work populates argument types later",
    );
}

#[test]
fn every_extracted_reference_has_exactly_one_stored_outcome() {
    // The non-negotiable, counted rather than asserted in prose: the three
    // outcome columns are a partition of the references the extractor found,
    // so they must sum to the reference count exactly. Over-counting is as
    // much a failure as dropping — one reference, one outcome.
    //
    // `local_binding` is one of the columns even though it is outside both
    // terms of the rate: excluded from the measurement, never from the
    // accounting. Leaving it out here is exactly how moving references into
    // it could pass for an improvement.
    //
    // Hand-counted for `fixture`:
    //   util/util.go      0 imports, 0 calls
    //                     2 type uses (`string` twice — the parameter and
    //                                  the result of `Parse`)
    //   server/server.go  2 imports ("fmt", "example.com/app/util")
    //                     6 calls   (fmt.Println, util.Parse, helper,
    //                                missing, conn.Close, pool.Close)
    //                     2 type uses (`Conn` in `var pool Conn` and in
    //                                  `Serve`'s signature; `type Conn
    //                                  struct{}` declares and does not name)
    const EXPECTED_REFERENCES: u64 = 12;

    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let report = scan_go(dir.path(), &dir.path().join("graph.redb")).expect("scan succeeds");
    let go = &report.per_lang[&Lang::Go.code()];

    assert_eq!(
        go.resolved + go.external + go.local_binding + go.unresolved_total(),
        EXPECTED_REFERENCES,
        "resolved {} + external {} + local-binding {} + unresolved {} must \
         account for every reference in the fixture, once each",
        go.resolved,
        go.external,
        go.local_binding,
        go.unresolved_total(),
    );
}

#[test]
fn a_local_and_a_package_level_call_of_one_name_are_two_rows() {
    // Same file, same enclosing function, same site text, same arity — and
    // two different outcomes, because an inner block binds the first and
    // only the second names the package-level function. Collapsing them
    // into one row keeps whichever outcome came first and attributes both
    // occurrences to it, moving a resolved reference into the local-binding
    // bucket without ever dropping a row: every count still sums, and the
    // rate is wrong in both terms.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        dir.path(),
        "p/p.go",
        concat!(
            "package p\n\n",
            "func x() {}\n\n",
            "func f() {\n",
            "\t{\n\t\tx := func() {}\n\t\tx()\n\t}\n",
            "\tx()\n",
            "}\n",
        ),
    );
    let db = dir.path().join("graph.redb");
    let report = scan_go(dir.path(), &db).expect("scan succeeds");
    let go_tally = &report.per_lang[&Lang::Go.code()];

    assert_eq!(
        go_tally.local_binding, 1,
        "exactly one call names the block-local",
    );
    assert_eq!(
        go_tally.resolved, 1,
        "exactly one call names the package-level x",
    );
    let store = Store::open(&db).expect("store opens");
    assert!(
        calls(&store, "example.com/app/p#f", "example.com/app/p#x"),
        "the package-level call is a real edge",
    );
}

#[test]
fn a_package_genuinely_named_like_a_test_package_lands_in_one_namespace() {
    // `weird/` declares `package api_test` in its production file, so
    // `api_test` is that directory's real package name and its `_test.go`
    // file is an ordinary in-package test — not an external test package.
    //
    // Both phases decide that by asking what the directory's package is
    // called, and they ask with different knowledge: phase 1 sees only the
    // container names the store held before the event, phase 2 sees the
    // ones phase 1 just wrote. On a cold scan phase 1 has no name for the
    // directory and falls back to `weird`, files the test file's
    // definitions under `weird#test`, and phase 2 — now knowing the name is
    // `api_test` — sources their edges at `weird`. One file, two
    // namespaces, and an edge from a node nothing declares.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "weird/prod.go",
        "package api_test\n\nfunc Helper() {}\n",
    );
    write(
        root,
        "weird/prod_test.go",
        "package api_test\n\nfunc TestX() {\n\tHelper()\n}\n",
    );
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("scan succeeds");
    let store = Store::open(&db).expect("store opens");

    assert!(
        node(&store, "example.com/app/weird#TestX").is_some(),
        "the test file shares its directory's package, so it is not `#test`",
    );
    assert_eq!(
        node(&store, "example.com/app/weird!test#TestX"),
        None,
        "an external test package is one whose name differs from the \
         directory's; here they are the same",
    );
    assert!(
        calls(
            &store,
            "example.com/app/weird#TestX",
            "example.com/app/weird#Helper",
        ),
        "the edge must start at the node the definition phase declared",
    );
}

#[test]
fn a_dotted_directory_name_cannot_collide_with_a_definition() {
    // A Go import path may carry a dot inside a path element — `gopkg.in`
    // and `yaml.v2` both do — so a directory may legitimately be named
    // `p.Foo`. Joining a container to its members with `.` then gives the
    // function `Foo` of package `example.com/m/p` and the package in
    // directory `p.Foo` the same FQN, and therefore the same node: one
    // record, silently overwritten by whichever was written last.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/m\n\ngo 1.22\n");
    write(root, "p/p.go", "package p\n\nfunc Foo() {}\n");
    write(root, "p.Foo/x.go", "package pfoo\n\nfunc Bar() {}\n");
    let db = root.join("graph.redb");
    scan_go(root, &db).expect("scan succeeds");
    let store = Store::open(&db).expect("store opens");

    let at_package_path = node(&store, "example.com/m/p.Foo");
    assert!(
        matches!(at_package_path, Some(NodeRecord::Package { .. })),
        "the package in directory `p.Foo` must keep its own identity, not \
         share one with the function `Foo` of package `p`: {at_package_path:?}",
    );
    let function = node(&store, "example.com/m/p#Foo");
    assert!(
        matches!(function, Some(NodeRecord::Definition { .. })),
        "the function has an identity of its own, and `#` is what keeps it \
         out of every import path's keyspace: {function:?}",
    );
    assert_eq!(
        store.report().expect("report").fqn_collisions,
        0,
        "a package and a function are not a definition collision; they are \
         two nodes that must never have met",
    );
}

#[test]
fn an_external_reference_gets_a_node_and_an_edge() {
    // A dependency outside the repository is a node like any other, so a
    // call into one is a real edge rather than a dead end — and giving it
    // one must not move a single tally, because the reference's outcome is
    // still `External` and the rate never counted it.
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let db = dir.path().join("graph.redb");

    let report = scan_go(dir.path(), &db).expect("scan succeeds");
    let tally = &report.per_lang[&Lang::Go.code()];
    assert_eq!(tally.resolved, 5);
    assert_eq!(tally.external, 4);
    assert_eq!(tally.local_binding, 1);
    assert_eq!(tally.unresolved_total(), 2);

    let store = Store::open(&db).expect("reopen");
    // The import of `"fmt"` and the call `fmt.Println` reach the standard
    // library under the two strings the resolver produces for them.
    for (fqn, package) in [("external:std:fmt", "std:fmt"), ("external:fmt", "fmt")] {
        match node(&store, fqn) {
            Some(NodeRecord::External {
                package: stored,
                declarations,
            }) => {
                assert_eq!(stored, package);
                assert_eq!(declarations.len(), 1, "one site per referencing file");
                assert_eq!(declarations[0].file, "server/server.go");
            }
            other => panic!("{fqn} should be an external node, not {other:?}"),
        }
    }
    // The import has no nameable encloser, so its edge starts at the file's
    // package; the call's starts at the function it sits in.
    assert!(links(
        &store,
        "example.com/app/server",
        "external:std:fmt",
        RefKind::Import,
    ));
    assert!(links(
        &store,
        "example.com/app/server#Serve",
        "external:fmt",
        RefKind::Call,
    ));

    // The prefix is what keeps the external keyspace unreachable: no Go
    // import path or FQN may contain a `:`, so no candidate can be built
    // that collides with one of these identities.
    assert_ne!(go("external:std:fmt"), go("std:fmt"));
    assert_eq!(node(&store, "std:fmt"), None);
    assert_eq!(node(&store, "fmt"), None);
}

#[test]
fn a_receiver_shadowing_an_import_resolves_to_its_own_type_not_the_import() {
    // The receiver `h` shadows `import h "net/http"` for the whole of
    // `Handle`, so `h.reset()` names the receiver's own method. Linking it to
    // the import is a wrong edge, and a wrong edge is strictly worse than an
    // unresolved reference: the miss would have been counted, the wrong edge
    // is not.
    //
    // It is not excluded to achieve that. A receiver is Go's `this`, so the
    // site resolves against the type the signature states — the same
    // declared-type lookup Java, Python, JavaScript and TypeScript run for
    // `this.reset()` — and lands on the right definition, in both terms of
    // the rate.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "server/server.go",
        concat!(
            "package server\n\n",
            "import h \"net/http\"\n\n",
            "type Handler struct{}\n\n",
            "func (h *Handler) reset() {}\n\n",
            "func (h *Handler) Handle() {\n",
            "\th.reset()\n",
            "}\n\n",
            "func Serve() {\n",
            "\th.ListenAndServe(\"\", nil)\n",
            "}\n",
        ),
    );
    let db = root.join("graph.redb");

    let report = scan_go(root, &db).expect("scan succeeds");
    let store = Store::open(&db).expect("reopen");
    assert!(
        !links(
            &store,
            "example.com/app/server#Handler.Handle",
            "external:net/http",
            RefKind::Call,
        ),
        "the receiver shadows the import: this edge is a lie",
    );
    assert!(
        links(
            &store,
            "example.com/app/server#Handler.Handle",
            "example.com/app/server#Handler.reset",
            RefKind::Call,
        ),
        "the receiver's own method is the edge, and it is a real one",
    );
    // One function away the same `h` really is the import, and that edge
    // must survive — the fix is a binding rule, not a blanket suppression.
    assert!(links(
        &store,
        "example.com/app/server#Serve",
        "external:net/http",
        RefKind::Call,
    ));

    let tally = &report.per_lang[&Lang::Go.code()];
    assert_eq!(
        tally.local_binding, 0,
        "a member selected through a receiver is never a local binding",
    );
    assert_eq!(tally.unresolved_total(), 0, "{:?}", tally.unresolved);
    assert_eq!(
        tally.resolved, 3,
        "`h.reset` and the two `Handler` receiver type uses",
    );
    assert_eq!(tally.external, 2, "the import and the genuine `h` use");
}

#[test]
fn local_binding_is_outside_both_rate_terms() {
    // One resolved, one unresolved, three local bindings: the rate is
    // exactly one half, because the local bindings are in neither term.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "app/a.go",
        concat!(
            "package app\n\n",
            "func helper() {}\n\n",
            "func Run(cb func(), other func()) {\n",
            "\thelper()\n",  // resolved
            "\tmissing()\n", // NoMatchingDefinition
            "\tcb()\n",      // parameter
            "\tother()\n",   // parameter
            "\tinner := cb\n",
            "\tinner()\n", // local
            "}\n",
        ),
    );

    let report = scan_go(root, &root.join("graph.redb")).expect("scan succeeds");
    let tally = &report.per_lang[&Lang::Go.code()];
    assert_eq!(tally.resolved, 1);
    assert_eq!(tally.external, 0);
    assert_eq!(tally.local_binding, 3);
    assert_eq!(tally.unresolved_total(), 1, "{:?}", tally.unresolved);
    let rate = arthron::resolution_rate(tally.resolved, tally.unresolved_total()).unwrap();
    assert!((rate - 0.5).abs() < 1e-9, "rate {rate}");
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

    let report = scan_go(root, &db).expect("scan succeeds");
    let tally = &report.per_lang[&Lang::Go.code()];
    // The import and the call, both resolved; nothing left over.
    assert_eq!(tally.resolved, 2);
    assert_eq!(tally.unresolved_total(), 0, "{:?}", tally.unresolved);

    // The import path stays the directory — only the binding name comes
    // from the declaration.
    let store = Store::open(&db).expect("reopen");
    assert!(calls(
        &store,
        "example.com/app/server#Serve",
        "example.com/app/utilx#Parse",
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

    let report = scan_go(root, &db).expect("scan succeeds");
    let tally = &report.per_lang[&Lang::Go.code()];
    // Both `setup()` calls resolve — one row per file.
    assert_eq!(tally.resolved, 2);
    assert_eq!(tally.unresolved_total(), 0, "{:?}", tally.unresolved);

    let store = Store::open(&db).expect("reopen");
    assert!(
        node(&store, "example.com/app/boot#init").is_none(),
        "init is not nameable, so it must not be a node"
    );
    assert!(node(&store, "example.com/app/boot#setup").is_some());
    assert!(
        calls(&store, "example.com/app/boot", "example.com/app/boot#setup"),
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

    let report = scan_go(root, &db).expect("scan succeeds");
    let tally = &report.per_lang[&Lang::Go.code()];
    // helperT(), graph.Build(), and the import of the production package.
    assert_eq!(tally.resolved, 3);
    assert_eq!(tally.unresolved_total(), 0, "{:?}", tally.unresolved);

    let store = Store::open(&db).expect("reopen");
    // `#` is forbidden in a Go module path, so `{dir}#test` is an identity
    // no real directory can claim — a sibling directory named `graph_test`
    // used to share this namespace.
    assert!(node(&store, "example.com/app/graph!test#helperT").is_some());
    assert!(
        node(&store, "example.com/app/graph_test#helperT").is_none(),
        "a directory named `graph_test` may exist; this namespace is not it"
    );
    assert!(
        node(&store, "example.com/app/graph#helperT").is_none(),
        "a test-file definition must not land in the production namespace"
    );
    assert!(
        node(&store, "example.com/app/graph!test#Run").is_some(),
        "every definition in the test file is namespaced, not just some"
    );
    // The production definition keeps its own FQN, and the test package
    // reaches it the ordinary way: through its import.
    assert!(node(&store, "example.com/app/graph#Build").is_some());
    assert!(calls(
        &store,
        "example.com/app/graph!test#Run",
        "example.com/app/graph#Build",
    ));
    assert!(calls(
        &store,
        "example.com/app/graph!test#Run",
        "example.com/app/graph!test#helperT",
    ));
}

#[test]
fn an_in_package_test_file_stays_in_the_production_namespace() {
    // `package graph` in `graph_test.go` is an in-package test: same
    // package, same namespace. Only a declared name that differs from the
    // directory's own — in a file that really is a test file — splits off.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(root, "graph/graph.go", "package graph\n\nfunc Build() {}\n");
    write(
        root,
        "graph/graph_test.go",
        "package graph\n\nfunc TestBuild() {\n\tBuild()\n}\n",
    );
    let db = root.join("graph.redb");

    scan_go(root, &db).expect("scan succeeds");
    let store = Store::open(&db).expect("reopen");
    assert!(node(&store, "example.com/app/graph#TestBuild").is_some());
    assert!(node(&store, "example.com/app/graph!test#TestBuild").is_none());
    assert!(calls(
        &store,
        "example.com/app/graph#TestBuild",
        "example.com/app/graph#Build",
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

    let first = scan_go(root, &db).expect("first scan");
    assert_eq!(first.per_lang[&Lang::Go.code()].resolved, 2);

    // Edit only the importing file. `utilx/util.go` keeps its hash, so it
    // is not extracted again.
    write(
        root,
        "server/server.go",
        &server("\t_ = s\n\treturn util.Parse(s)\n"),
    );
    let second = scan_go(root, &db).expect("second scan");
    let tally = &second.per_lang[&Lang::Go.code()];
    assert_eq!(tally.resolved, 2);
    assert_eq!(tally.unresolved_total(), 0, "{:?}", tally.unresolved);
}

#[test]
fn second_scan_of_unchanged_tree_reports_the_same() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let db = dir.path().join("graph.redb");
    let first = scan_go(dir.path(), &db).expect("first scan");
    // Warm path: every file hash matches, the changed set is empty, and
    // the report must come from the store, unchanged.
    let second = scan_go(dir.path(), &db).expect("second scan");
    assert_eq!(first.per_lang, second.per_lang);
}

#[test]
fn a_tree_with_none_of_a_languages_files_is_not_an_error() {
    // A Go-less repository owes nobody a go.mod: the scan must return, not
    // fail reading a manifest that has no reason to exist. This is what lets
    // `scan_repo` run every live track over a single-language repository.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "app/greet.py", "def hi():\n    hi()\n");

    let report = scan_go(root, &root.join("graph.redb")).expect("a fileless scan succeeds");
    assert!(
        !report.per_lang.contains_key(&Lang::Go.code()),
        "a language that read nothing reported a line",
    );
}

#[test]
fn a_language_whose_files_all_vanished_is_forgotten_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let db = root.join("graph.redb");
    write(root, "go.mod", "module example.com/app\n");
    write(root, "app/app.go", "package app\n\nfunc Run() { Run() }\n");
    scan_go(root, &db).expect("first scan");

    fs::remove_file(root.join("app/app.go")).unwrap();
    fs::remove_file(root.join("go.mod")).unwrap();
    let report = scan_go(root, &db).expect("the scan after deletion succeeds");
    assert!(!report.per_lang.contains_key(&Lang::Go.code()));
    let store = Store::open(&db).expect("store opens");
    assert!(
        store.known_files().expect("known files").is_empty(),
        "the vanished language's files were not forgotten",
    );
}
