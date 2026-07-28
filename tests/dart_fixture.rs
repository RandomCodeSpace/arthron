//! The Dart track end to end, over a multi-package repository written to disk.
//!
//! `tests/dart_resolve.rs` drives the resolver directly against a symbol table
//! a test hands it, and `tests/dart_corpus.rs` measures a single-package
//! corpus. Neither reaches the layer this file exists for: whether the *walk*
//! and the *manifest* agree about a package that lives in a subdirectory. A
//! `path:` dependency is a fact stated in one file about the location of
//! another, and the only way to be sure the two halves meet is to write the
//! tree and scan it.
//!
//! What it pins is the second half of the laundering defence stated in
//! `src/track_dart/resolve.rs`: a `package:` URI naming a package this
//! repository *contains* is a lookup that can miss, never an `External` that
//! cannot. In a workspace the cross-package imports are the linking that
//! matters, so answering them `External` would leave a track that links
//! nothing between packages printing a full rate.
//!
//! Needs no corpus: the fixture is written here, so this runs everywhere.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::model::{Lang, NodeId, RefKind, reason_name};
use arthron::store::{NodeRecord, ReadStore, StoredOutcome};
use arthron::track_dart::resolve::scan_dart;

/// Write a file, creating its parent directories.
fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
}

/// A root package with one member package beside it, reached the way pub
/// reaches one — and one dependency that really is outside.
fn repository(root: &Path) {
    write(
        root,
        "pubspec.yaml",
        "name: rootpkg\n\
         dependencies:\n  \
           member:\n    path: pkgs/member\n  \
           http: ^1.0.0\n\
         dev_dependencies:\n  \
           test: ^1.16.0\n",
    );
    write(
        root,
        "lib/main.dart",
        "import 'package:member/member.dart';\n\
         import 'package:member/src/deep.dart';\n\
         import 'package:member/gone.dart';\n\
         import 'package:rootpkg/util.dart';\n\
         import 'package:http/http.dart';\n\
         import 'dart:math';\n\
         class App {}\n",
    );
    write(root, "lib/util.dart", "int two() => 2;\n");
    // The member package: its own manifest is not read, and does not need to
    // be — the root placed it, and `lib/` is where `package:member/…` looks.
    write(root, "pkgs/member/pubspec.yaml", "name: member\n");
    write(
        root,
        "pkgs/member/lib/member.dart",
        "export 'src/deep.dart';\nclass Member {}\n",
    );
    write(root, "pkgs/member/lib/src/deep.dart", "class Deep {}\n");
}

/// Every stored row, keyed by `(file, raw target)`.
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
            let shown = match record.outcome {
                StoredOutcome::Resolved(id) => nodes
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| "resolved:<unknown node>".to_string()),
                StoredOutcome::External(pkg) => format!("external:{pkg}"),
                StoredOutcome::Unresolved(code) => format!("unresolved:{}", reason_name(code)),
            };
            assert!(
                key.kind == RefKind::Import.code() || key.kind == RefKind::Export.code(),
                "tier 2 stores the library directives and nothing else",
            );
            out.insert((key.file, key.raw_target), shown);
            Ok(())
        })
        .expect("rows");
    out
}

#[test]
fn a_package_the_manifest_places_inside_this_repository_is_linked_and_not_laundered() {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let root = scratch.path();
    repository(root);
    let db = root.join("graph.redb");
    let report = scan_dart(root, &db).expect("the repository scans");
    let tally = report
        .per_lang
        .get(&Lang::Dart.code())
        .cloned()
        .unwrap_or_default();

    let rows = rows(&db);
    let got = |file: &str, raw: &str| {
        rows.get(&(file.to_string(), raw.to_string()))
            .unwrap_or_else(|| panic!("no row for `{raw}` in {file}; rows: {rows:#?}"))
            .as_str()
    };

    // The reference this file exists for: a `path:` dependency's `package:`
    // URI reaches the member's `lib/`, and the target is the very node the
    // walk stored for that file.
    assert_eq!(
        got("lib/main.dart", "import 'package:member/member.dart'"),
        "$pkgs/member/lib/member.dart",
    );
    // Including below it — `package:<name>/<path>` is a path under `lib/`,
    // not a flat name.
    assert_eq!(
        got("lib/main.dart", "import 'package:member/src/deep.dart'"),
        "$pkgs/member/lib/src/deep.dart",
    );
    // And a member file's own relative export resolves inside its package.
    assert_eq!(
        got("pkgs/member/lib/member.dart", "export 'src/deep.dart'"),
        "$pkgs/member/lib/src/deep.dart",
    );
    // A miss inside the member is a miss, counted — the laundering this
    // ordering exists to prevent would have made it `external:member`.
    assert_eq!(
        got("lib/main.dart", "import 'package:member/gone.dart'"),
        "unresolved:ModuleNotFound",
    );
    // The root's own package is unaffected, and a dependency the manifest
    // does *not* place inside the tree still leaves it.
    assert_eq!(
        got("lib/main.dart", "import 'package:rootpkg/util.dart'"),
        "$lib/util.dart",
    );
    assert_eq!(
        got("lib/main.dart", "import 'package:http/http.dart'"),
        "external:http",
    );
    assert_eq!(
        got("lib/main.dart", "import 'dart:math'"),
        "external:dart:math",
    );

    // Four in-repository lookups — three hits and one miss — plus the member's
    // own export, against two names that really are outside.
    assert_eq!(
        (
            tally.resolved,
            tally.external,
            tally.local_binding,
            tally.unresolved_total(),
        ),
        (4, 2, 0, 1),
    );
}
