//! Haskell resolution end to end: a scan of a synthetic multi-package tree,
//! through phase 0, the definition phase and the resolver, into a store.
//!
//! The fixtures in `src/track_haskell/resolve.rs` put a resolver in front of a
//! hand-built symbol table. These put it behind the real driver, because the
//! three facts that decide a Haskell import — which `.cabal` files were found,
//! which module names the walk saw, and which file a module node was minted
//! for — are all produced by layers a unit test stands in for.

use std::path::Path;

use arthron::model::{Domain, Lang, node_id, reason_name};
use arthron::store::{ReadStore, Report};
use arthron::track_haskell::resolve::scan_haskell;

/// Write a file, creating the directories above it.
fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("mkdir");
    }
    std::fs::write(path, body).expect("write");
}

/// Scan a tree into a fresh store and hand back the report and the store path.
fn scan(root: &Path, db: &Path) -> Report {
    scan_haskell(root, db).expect("the tree scans")
}

/// `(resolved, external, local_binding, unresolved)` for Haskell.
fn counts(report: &Report) -> (u64, u64, u64, u64) {
    let t = report
        .per_lang
        .get(&Lang::Haskell.code())
        .cloned()
        .unwrap_or_default();
    (
        t.resolved,
        t.external,
        t.local_binding,
        t.unresolved_total(),
    )
}

/// Every unresolved reason the report carries, by name.
fn reasons(report: &Report) -> Vec<(String, u64)> {
    let t = report
        .per_lang
        .get(&Lang::Haskell.code())
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<(String, u64)> = t
        .unresolved
        .iter()
        .map(|(c, n)| (reason_name(*c).to_string(), *n))
        .collect();
    out.sort();
    out
}

#[test]
fn a_module_is_found_under_the_root_its_own_components_manifest_declares() {
    let dir = tempfile::tempdir().expect("scratch");
    let root = dir.path();
    write(
        root,
        "app.cabal",
        "name: app\nlibrary\n  hs-source-dirs: src\n  build-depends: base, helper\n\
         test-suite spec\n  hs-source-dirs: tests\n  build-depends: app\n",
    );
    write(
        root,
        "helper/helper.cabal",
        "name: helper\nlibrary\n  hs-source-dirs: lib\n  build-depends: base\n",
    );
    write(root, "src/App/Core.hs", "module App.Core where\nx = 1\n");
    write(
        root,
        "src/App.hs",
        "module App where\nimport App.Core\nimport Helper.Util\nimport Data.Text\n",
    );
    write(
        root,
        "helper/lib/Helper/Util.hs",
        "module Helper.Util where\ny = 2\n",
    );
    write(
        root,
        "tests/Spec.hs",
        "module Main where\nimport App\nmain = pure ()\n",
    );

    let db = root.join("graph.redb");
    let report = scan(root, &db);
    // `App.Core` under `src`, `Helper.Util` under `helper/lib`, `App` from the
    // test tree: three roots, three components, one measurement.
    assert_eq!(counts(&report), (3, 1, 0, 0));
    assert!(reasons(&report).is_empty(), "{:?}", reasons(&report));

    let read = ReadStore::open(&db).expect("store opens");
    for fqn in [
        "src/App",
        "src/App/Core",
        "helper/lib/Helper/Util",
        "tests/Spec",
    ] {
        let id = node_id(Domain::Haskell, fqn);
        assert!(
            arthron::query::definition(&read, &id)
                .expect("query")
                .is_some(),
            "{fqn} is not a node",
        );
    }
}

#[test]
fn a_module_the_repository_declares_is_never_external_when_its_root_is_unread() {
    // The laundering shape, end to end. `Helper.Util` really is in the tree —
    // the walk read the file and minted its node — but no manifest declares
    // `helper/lib` as a source root, so no candidate path reaches it. The
    // reference must land inside the denominator as this build's own layout
    // failure, not outside both rate terms as `External`.
    let dir = tempfile::tempdir().expect("scratch");
    let root = dir.path();
    write(
        root,
        "app.cabal",
        "name: app\nlibrary\n  hs-source-dirs: src\n  build-depends: base\n",
    );
    // `helper/` has no manifest at all: its source root is invisible to phase 0.
    write(
        root,
        "helper/lib/Helper/Util.hs",
        "module Helper.Util where\ny = 2\n",
    );
    write(
        root,
        "src/App.hs",
        "module App where\nimport Helper.Util\nimport Data.Text\n",
    );

    let db = root.join("graph.redb");
    let report = scan(root, &db);
    assert_eq!(counts(&report), (0, 1, 0, 1));
    assert_eq!(
        reasons(&report),
        [("ProjectLayoutUnknown".to_string(), 1)],
        "an in-repository module was classified as something else",
    );
}

#[test]
fn a_tree_with_no_manifest_reports_a_layout_failure_and_no_external() {
    // Without a `.cabal` nothing is a home module, so an unguarded rule would
    // call the entire denominator external and report a rate over nothing.
    let dir = tempfile::tempdir().expect("scratch");
    let root = dir.path();
    write(
        root,
        "src/App.hs",
        "module App where\nimport App.Core\nimport Data.Text\n",
    );
    write(root, "src/App/Core.hs", "module App.Core where\nx = 1\n");

    let db = root.join("graph.redb");
    let report = scan(root, &db);
    assert_eq!(counts(&report), (0, 0, 0, 2));
    assert_eq!(reasons(&report), [("ProjectLayoutUnknown".to_string(), 2)]);
}

#[test]
fn a_repository_that_names_no_outside_dependency_mints_no_external() {
    let dir = tempfile::tempdir().expect("scratch");
    let root = dir.path();
    write(
        root,
        "app.cabal",
        "name: app\nlibrary\n  hs-source-dirs: src\n",
    );
    write(root, "src/App.hs", "module App where\nimport Data.Text\n");

    let db = root.join("graph.redb");
    let report = scan(root, &db);
    assert_eq!(counts(&report), (0, 0, 0, 1));
    assert_eq!(reasons(&report), [("UnknownPackage".to_string(), 1)]);
}

#[test]
fn two_files_declaring_one_module_name_stay_two_nodes() {
    // The measured corpus's six `module Main` executables, in miniature. A
    // name-keyed identity would merge them; nothing here does, and the report
    // records no collision because a module is a package node.
    let dir = tempfile::tempdir().expect("scratch");
    let root = dir.path();
    write(
        root,
        "app.cabal",
        "name: app\nexecutable one\n  hs-source-dirs: one\n  main-is: Main.hs\n\
         executable two\n  hs-source-dirs: two\n  main-is: Main.hs\n  build-depends: base\n",
    );
    write(root, "one/Main.hs", "module Main where\nmain = pure ()\n");
    write(root, "two/Main.hs", "module Main where\nmain = pure ()\n");

    let db = root.join("graph.redb");
    let report = scan(root, &db);
    assert_eq!(report.fqn_collisions, 0);

    let read = ReadStore::open(&db).expect("store opens");
    for fqn in ["one/Main", "two/Main"] {
        let id = node_id(Domain::Haskell, fqn);
        assert!(
            arthron::query::definition(&read, &id)
                .expect("query")
                .is_some(),
            "{fqn} is not a node",
        );
    }
    // And each file's `main` is its own function, not one shared identity.
    for fqn in ["one/Main#main", "two/Main#main"] {
        let id = node_id(Domain::Haskell, fqn);
        let def = arthron::query::definition(&read, &id)
            .expect("query")
            .unwrap_or_else(|| panic!("{fqn} is not a node"));
        assert_eq!(def.declarations.len(), 1, "{fqn}");
    }
}

#[test]
fn a_module_reachable_under_two_roots_binds_to_the_importing_files_own() {
    // The symlink shape the corpus carries, written out longhand: one module
    // name under two roots, and each package's import taking its own copy.
    let dir = tempfile::tempdir().expect("scratch");
    let root = dir.path();
    write(
        root,
        "a.cabal",
        "name: a\nlibrary\n  hs-source-dirs: asrc\n  build-depends: base\n",
    );
    write(
        root,
        "b/b.cabal",
        "name: b\nlibrary\n  hs-source-dirs: src\n  build-depends: base\n",
    );
    write(root, "asrc/Shared.hs", "module Shared where\nx = 1\n");
    write(root, "b/src/Shared.hs", "module Shared where\nx = 1\n");
    write(root, "asrc/A.hs", "module A where\nimport Shared\n");
    write(root, "b/src/B.hs", "module B where\nimport Shared\n");

    let db = root.join("graph.redb");
    let report = scan(root, &db);
    assert_eq!(counts(&report), (2, 0, 0, 0));

    let read = ReadStore::open(&db).expect("store opens");
    for (module, want) in [("b/src/Shared", "b/src/B.hs"), ("asrc/Shared", "asrc/A.hs")] {
        let sites =
            arthron::query::references(&read, &node_id(Domain::Haskell, module)).expect("refs");
        let from: Vec<(&str, &str)> = sites
            .iter()
            .map(|s| (s.file.as_str(), s.enclosing.as_str()))
            .collect();
        assert_eq!(
            from.len(),
            1,
            "{module} is named by more than its own package: {from:?}",
        );
        assert_eq!(from[0].0, want, "{module}");
    }
}
