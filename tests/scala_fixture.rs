//! The Scala track end to end, over a tree written to disk.
//!
//! The inline tests in `src/track_scala/` drive the extractor and the
//! resolver directly, against a symbol table a test hands them. This file
//! drives the **whole track** — both phases, the identity the definition
//! phase filed and the identity the reference phase probed, the store — and
//! it is the only place those two can be caught disagreeing.
//!
//! Needs no corpus: the fixture is written here, so this runs everywhere.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::model::{Lang, NodeId, RefKind, reason_name};
use arthron::store::{NodeRecord, ReadStore, StoredOutcome};
use arthron::track_scala::resolve::scan_scala;

/// Write a file, creating its parent directories.
fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
}

/// A tree exercising every shape the resolver has a rule for, laid out the
/// way a cross-built Scala repository is: one package written from several
/// source roots, and two names — a `trait` and an `object` — each written
/// twice under two of them. The pair is deliberate: the two take different
/// paths through the store, and only the `object` can be lost to the package
/// nodes.
fn tree(root: &Path) {
    write(
        root,
        "core/src/upickle/core/Visitor.scala",
        "package upickle.core\n\
         trait Visitor[T]\n\
         object Visitor { class Delegate }\n\
         object compat { def toIterator(): Int = 1 }\n",
    );
    write(
        root,
        "core/src/upickle/core/Use.scala",
        // A relative import into the enclosing package's own member.
        "package upickle.core\nimport compat._\nclass User\n",
    );
    write(
        root,
        "impl/src/upickle/implicits/Api.scala",
        // Absolute, a selector list, a rename, a wildcard, and a name that is
        // not there.
        "package upickle.implicits\n\
         import upickle.core.{Visitor, Delegate => D}\n\
         import upickle.core.Visitor.Delegate\n\
         import upickle.core._\n\
         import upickle.core.Absent\n\
         import upickle.absent.Thing\n\
         import java.nio.ByteBuffer\n\
         trait Api\n",
    );
    write(
        root,
        "test/src/upickletest/TestUtil.scala",
        "package upickletest\nobject TestUtil { def rw(): Int = 1 }\nobject Helpers\n",
    );
    write(
        root,
        "test/src/upickletest/example/Example.scala",
        // A qualified package clause opens one scope, so `Helpers` is not in
        // scope here — but `TestUtil`, bound by a top-level import above,
        // is.
        "package upickletest.example\n\
         import upickletest.TestUtil\n\
         import Helpers._\n\
         object Example {\n\
         \x20 import TestUtil.rw\n\
         }\n",
    );
    write(
        root,
        "test/src-2/upickletest/example/Two.scala",
        // Written as two clauses: `upickletest`'s members *are* in scope.
        "package upickletest\npackage example\nimport Helpers._\nclass Two\n",
    );
    // The cross-build pair: one FQN, two source roots, both real.
    write(
        root,
        "js/src-js/upickle/WebJson.scala",
        "package upickle\ntrait WebJson\n",
    );
    write(
        root,
        "jvm/src-jvm/upickle/WebJson.scala",
        "package upickle\ntrait WebJson\n",
    );
    // The same shape written as an `object`, and deliberately with no member
    // — an object is a container in the FQN grammar, so a track that filed it
    // as a *package* node would store one node with two declaration sites,
    // count no collision, and never ask `mergeable` about it. With no member
    // to be counted in its place, that loss is total and this is the only
    // assertion that can see it.
    write(
        root,
        "js/src-js/upickle/Platform.scala",
        "package upickle\nobject Platform\n",
    );
    write(
        root,
        "jvm/src-jvm/upickle/Platform.scala",
        "package upickle\nobject Platform\n",
    );
    // A file in the unnamed root package, and a `package object`.
    write(
        root,
        "json/src/ujson/package.scala",
        "import upickle.core.Visitor\npackage object ujson { def read(): Int = 1 }\n",
    );
    write(
        root,
        "json/src/ujson/Value.scala",
        "package ujson\nimport ujson.read\nclass Value\n",
    );
    // Build output: never descended into, so nothing here is a definition
    // and nothing here resolves.
    write(
        root,
        "out/generated/upickle/Generated.scala",
        "package upickle\ntrait Generated\n",
    );
}

/// Every stored row, keyed by `(file, raw target)`, showing what it resolved
/// to or why it did not.
fn rows(db: &Path) -> BTreeMap<(String, String), String> {
    let store = ReadStore::open(db).expect("the store opens");
    let mut nodes: BTreeMap<NodeId, String> = BTreeMap::new();
    store
        .for_each_node(|id, record| {
            let fqn = match record {
                NodeRecord::Package { import_path, .. } => import_path,
                NodeRecord::Definition { fqn, .. } => fqn,
                NodeRecord::External { package, .. } => format!("external:{package}"),
            };
            nodes.insert(id, fqn);
            Ok(())
        })
        .expect("nodes");
    let mut out = BTreeMap::new();
    store
        .for_each_row(|key, record| {
            // The tier-2 contract at the store level: every stored row is an
            // import reference and none is a local binding.
            assert_eq!(key.kind, RefKind::Import.code(), "{key:?}");
            assert!(!key.locally_bound, "{key:?}");
            let shown = match record.outcome {
                StoredOutcome::Resolved(id) => nodes
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| "resolved:<unknown node>".to_string()),
                StoredOutcome::External(pkg) => format!("external:{pkg}"),
                StoredOutcome::Unresolved(code) => format!("unresolved:{}", reason_name(code)),
            };
            out.insert((key.file, key.raw_target), shown);
            Ok(())
        })
        .expect("rows");
    out
}

#[test]
fn the_scala_track_resolves_a_cross_built_tree_end_to_end() {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let root = scratch.path();
    tree(root);
    let db = root.join("graph.redb");
    let report = scan_scala(root, &db).expect("the tree scans");
    let tally = report
        .per_lang
        .get(&Lang::Scala.code())
        .cloned()
        .unwrap_or_default();

    let rows = rows(&db);
    for ((file, raw), shown) in &rows {
        println!("{file:52} {raw:44} {shown}");
    }

    let got = |file: &str, raw: &str| {
        rows.get(&(file.to_string(), raw.to_string()))
            .unwrap_or_else(|| panic!("no row for {file} {raw}"))
            .as_str()
    };

    // -- the path model ---------------------------------------------------

    // A relative import into the enclosing package's own member.
    assert_eq!(
        got("core/src/upickle/core/Use.scala", "compat._"),
        "_root_.upickle.core.compat",
    );
    // Absolute, through packages to a trait; and to an object beside it.
    let api = "impl/src/upickle/implicits/Api.scala";
    assert_eq!(
        got(api, "upickle.core.Visitor"),
        "_root_.upickle.core.Visitor"
    );
    assert_eq!(
        got(api, "upickle.core.Visitor.Delegate"),
        "_root_.upickle.core.Visitor#Delegate",
    );
    // A wildcard names the container and nothing it forwards.
    assert_eq!(got(api, "upickle.core._"), "_root_.upickle.core");
    // A rename names the original, which is not there.
    assert_eq!(
        got(api, "upickle.core.Delegate => D"),
        "unresolved:NoMatchingDefinition",
    );
    // A complete container without the name, versus a path that never
    // reached its container.
    assert_eq!(
        got(api, "upickle.core.Absent"),
        "unresolved:NoMatchingDefinition"
    );
    assert_eq!(
        got(api, "upickle.absent.Thing"),
        "unresolved:ModuleNotFound"
    );
    // Outside the repository. Never `External`: this track mints none.
    assert_eq!(got(api, "java.nio.ByteBuffer"), "unresolved:UnknownPackage");

    // -- the scoping rule -------------------------------------------------

    let example = "test/src/upickletest/example/Example.scala";
    // `package upickletest.example` does not put `upickletest`'s members in
    // scope, so `Helpers` leaves the repository...
    assert_eq!(got(example, "Helpers._"), "unresolved:UnknownPackage");
    // ...while `package upickletest` + `package example` does.
    assert_eq!(
        got("test/src-2/upickletest/example/Two.scala", "Helpers._"),
        "_root_.upickletest.Helpers",
    );
    // A top-level import binds a name for the rest of the file, including
    // inside a nested object.
    assert_eq!(
        got(example, "upickletest.TestUtil"),
        "_root_.upickletest.TestUtil",
    );
    assert_eq!(
        got(example, "TestUtil.rw"),
        "_root_.upickletest.TestUtil#rw"
    );

    // -- the root package -------------------------------------------------

    // A file with no `package` clause still has a container, and a
    // `package object` in it declares a real package other files import.
    assert_eq!(
        got("json/src/ujson/package.scala", "upickle.core.Visitor"),
        "_root_.upickle.core.Visitor",
    );
    assert_eq!(
        got("json/src/ujson/Value.scala", "ujson.read"),
        "_root_.ujson#read"
    );

    // -- build output is not source ---------------------------------------

    // `out/` is never descended into, so `upickle.Generated` is in no
    // symbol table and no file claims to declare it.
    let store = ReadStore::open(&db).expect("the store opens");
    let mut generated = false;
    let mut cross_built: BTreeMap<String, usize> = BTreeMap::new();
    let mut multi_file_packages: Vec<String> = Vec::new();
    let mut root_package = false;
    store
        .for_each_node(|_, record| {
            match &record {
                NodeRecord::Definition {
                    fqn, declarations, ..
                } => {
                    if fqn.contains("Generated") {
                        generated = true;
                    }
                    let files: std::collections::BTreeSet<&str> =
                        declarations.iter().map(|d| d.file.as_str()).collect();
                    if files.len() > 1 {
                        cross_built.insert(fqn.clone(), files.len());
                    }
                }
                NodeRecord::Package {
                    import_path,
                    declarations,
                    ..
                } => {
                    if import_path == "_root_" {
                        root_package = true;
                    }
                    let files: std::collections::BTreeSet<&str> =
                        declarations.iter().map(|d| d.file.as_str()).collect();
                    if files.len() > 1 {
                        multi_file_packages.push(import_path.clone());
                    }
                }
                NodeRecord::External { .. } => panic!("this track mints no external node"),
            }
            Ok(())
        })
        .expect("nodes");
    drop(store);
    assert!(!generated, "a file under out/ was indexed");
    assert!(root_package, "no node for the unnamed package");

    // -- the union over build configurations ------------------------------

    // Two FQNs, two source roots each, all four real under their own
    // platform. The graph holds the union and *says so* — `fqn_collisions`
    // is the report line that says it, and a resolver that merged either
    // pair would report one or zero.
    //
    // The `object` is the half that is easy to lose: it shares
    // `DefKind::Module` with a package, and a track that filed it as a
    // package node would keep both declaration sites and still count
    // nothing, because a package declared by every file under it is what a
    // package is. `Resolver::stores_as_package` is where the two part.
    let want: BTreeMap<String, usize> = [
        ("_root_.upickle#WebJson", 2),
        ("_root_.upickle.Platform", 2),
    ]
    .into_iter()
    .map(|(f, n)| (f.to_string(), n))
    .collect();
    assert_eq!(
        cross_built, want,
        "the cross-built pairs did not survive as two apiece"
    );
    assert_eq!(report.fqn_collisions, 2);

    // And the other side of the same rule, so that "count the containers"
    // never turns into "count the packages". Five packages here are written
    // by more than one file — `upickle` by the four cross-built files above,
    // `ujson` by a `package object` and a `package` clause — and not one of
    // them is a collision, because being reopened by every file under it is
    // what a package is.
    multi_file_packages.sort();
    assert_eq!(
        multi_file_packages,
        [
            "_root_.ujson",
            "_root_.upickle",
            "_root_.upickle.core",
            "_root_.upickletest",
            "_root_.upickletest.example",
        ],
        "a package several files declare is a package, not a collision",
    );

    // -- the tier ---------------------------------------------------------

    assert_eq!(
        tally.local_binding, 0,
        "tier 2 has nothing a local can bind"
    );
    assert_eq!(tally.external, 0, "this track mints no external node");
    assert!(tally.resolved > 0 && tally.unresolved_total() > 0);
}
