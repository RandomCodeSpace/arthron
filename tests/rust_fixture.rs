//! The Rust track end to end, over a workspace written to disk.
//!
//! The inline tests in `src/track_rust/` drive the extractor and the resolver
//! directly, against a symbol table a test hands them. This file drives the
//! **whole track** — manifest reading, the module tree, both phases, the
//! store — because the layer those tests cannot reach is exactly the one Rust
//! puts the most in: a crate root is a fact in `Cargo.toml`, and a module's
//! name is a fact about where its file sits relative to one.
//!
//! Needs no corpus: the fixture is written here, so this runs everywhere.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::model::{Lang, NodeId, RefKind, reason_name};
use arthron::store::{NodeRecord, ReadStore, StoredOutcome};
use arthron::track_rust::resolve::scan_rust;

/// Write a file, creating its parent directories.
fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
}

/// A two-crate workspace exercising every shape the resolver has a rule for.
fn workspace(root: &Path) {
    write(
        root,
        "Cargo.toml",
        r#"
[package]
name = "app"
edition = "2024"
autotests = false

[[bin]]
name = "app"
path = "src/main.rs"

[[test]]
name = "integration"
path = "tests/tests.rs"

[dependencies]
lib-one = { version = "0.1", path = "crates/one" }
serde = "1"
"#,
    );
    write(
        root,
        "src/main.rs",
        "mod util;\nmod nested;\nmod absent;\n\
         use crate::util::helper;\n\
         use crate::nested::deep::Deep;\n\
         use lib_one::Exported;\n\
         use serde::Serialize;\n\
         use std::io::Write;\n\
         fn main() {}\n",
    );
    write(root, "src/util.rs", "pub fn helper() {}\n");
    write(
        root,
        "src/nested/mod.rs",
        "pub mod deep;\nuse super::util::helper;\n",
    );
    write(
        root,
        "src/nested/deep.rs",
        "pub struct Deep;\n\
         use super::super::util::helper;\n\
         pub enum Kind { A, B }\n\
         use self::Kind::*;\n\
         mod inner { use super::Kind::*; use super::Deep; }\n",
    );
    write(root, "tests/tests.rs", "use app::nothing;\nmod support;\n");
    write(root, "tests/support.rs", "use std::fmt;\n");

    write(
        root,
        "crates/one/Cargo.toml",
        "[package]\nname = \"lib-one\"\nedition = \"2024\"\n",
    );
    write(
        root,
        "crates/one/src/lib.rs",
        "mod hidden;\npub use crate::hidden::Exported;\nuse nowhere::Thing;\n",
    );
    write(root, "crates/one/src/hidden.rs", "pub struct Exported;\n");
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
            assert_eq!(
                key.kind,
                RefKind::Import.code(),
                "tier 2 stores import references and nothing else",
            );
            out.insert((key.file, key.raw_target), shown);
            Ok(())
        })
        .expect("rows");
    out
}

#[test]
fn the_track_resolves_a_workspace_the_way_cargo_lays_one_out() {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let root = scratch.path();
    workspace(root);
    let db = root.join("graph.redb");
    let report = scan_rust(root, &db).expect("the workspace scans");
    let tally = report
        .per_lang
        .get(&Lang::Rust.code())
        .cloned()
        .unwrap_or_default();
    println!(
        "resolved {} external {} local-binding {} unresolved {}",
        tally.resolved,
        tally.external,
        tally.local_binding,
        tally.unresolved_total(),
    );
    for (code, count) in &tally.unresolved {
        println!("  {} {count}", reason_name(*code));
    }

    let rows = rows(&db);
    let got = |file: &str, raw: &str| {
        rows.get(&(file.to_string(), raw.to_string()))
            .unwrap_or_else(|| panic!("no row for `{raw}` in {file}; rows: {rows:#?}"))
            .as_str()
    };

    // A `mod` declaration names the module its own file declares — the
    // binary's crate root is `src/main.rs`, which the manifest states and no
    // directory implies.
    assert_eq!(got("src/main.rs", "mod util"), "src/main.rs::util");
    // `foo/mod.rs` is module `foo`.
    assert_eq!(got("src/main.rs", "mod nested"), "src/main.rs::nested");
    // No file, so no module, and the reason says which of the two it is.
    assert_eq!(
        got("src/main.rs", "mod absent"),
        "unresolved:ModuleNotFound"
    );

    // `crate::` roots at the target's root file, not at `src/`.
    assert_eq!(
        got("src/main.rs", "crate::util::helper"),
        "src/main.rs::util#helper"
    );
    assert_eq!(
        got("src/main.rs", "crate::nested::deep::Deep"),
        "src/main.rs::nested::deep#Deep"
    );

    // `super` climbs one module per keyword, from wherever the file sits.
    assert_eq!(
        got("src/nested/mod.rs", "super::util::helper"),
        "src/main.rs::util#helper"
    );
    assert_eq!(
        got("src/nested/deep.rs", "super::super::util::helper"),
        "src/main.rs::util#helper"
    );

    // A glob over an enum reads the item table: `use self::Kind::*` is
    // ordinary Rust and names no module at all.
    assert_eq!(
        got("src/nested/deep.rs", "self::Kind::*"),
        "src/main.rs::nested::deep#Kind"
    );
    // `self` and `super` are relative to the module the site sits in, and an
    // inline `mod` block moves that: from inside `inner`, `super` is `deep`.
    assert_eq!(
        got("src/nested/deep.rs", "super::Kind::*"),
        "src/main.rs::nested::deep#Kind"
    );
    assert_eq!(
        got("src/nested/deep.rs", "super::Deep"),
        "src/main.rs::nested::deep#Deep"
    );

    // A `path = …` dependency roots at the sibling crate's library, and the
    // name it reaches is the re-export — a real declaration site.
    assert_eq!(
        got("src/main.rs", "lib_one::Exported"),
        "crates/one/src/lib.rs#Exported"
    );
    // A declared dependency and the sysroot leave the repository.
    assert_eq!(got("src/main.rs", "serde::Serialize"), "external:serde");
    assert_eq!(got("src/main.rs", "std::io::Write"), "external:std");
    // A crate no manifest declares is an unknown package, not a missing name.
    assert_eq!(
        got("crates/one/src/lib.rs", "nowhere::Thing"),
        "unresolved:UnknownPackage"
    );

    // A test target names its own package's library by the package's name —
    // except this package has no library, so the name reaches nothing.
    assert_eq!(
        got("tests/tests.rs", "app::nothing"),
        "unresolved:UnknownPackage"
    );
    // `autotests = false` plus one explicit `[[test]]` makes the sibling a
    // *module* of the integration test rather than a crate root of its own.
    assert_eq!(
        got("tests/tests.rs", "mod support"),
        "tests/tests.rs::support"
    );
    assert_eq!(got("tests/support.rs", "std::fmt"), "external:std");

    // Nothing is dropped: every reference the extractor emitted has exactly
    // one outcome, and the four buckets account for all of them.
    let accounted =
        tally.resolved + tally.external + tally.local_binding + tally.unresolved_total();
    let mut extracted = 0u64;
    let store = ReadStore::open(&db).expect("the store opens");
    drop(store);
    for rel in [
        "src/main.rs",
        "src/util.rs",
        "src/nested/mod.rs",
        "src/nested/deep.rs",
        "tests/tests.rs",
        "tests/support.rs",
        "crates/one/src/lib.rs",
        "crates/one/src/hidden.rs",
    ] {
        let source = std::fs::read_to_string(root.join(rel)).expect("re-read");
        extracted += arthron::track_rust::extract::extract(rel, &source)
            .refs
            .len() as u64;
    }
    assert_eq!(accounted, extracted, "a reference went missing");

    // Tier 2 binds no local, and this is the contract rather than an empty
    // bucket nobody filled.
    assert_eq!(tally.local_binding, 0);
}

#[test]
fn a_stray_file_no_manifest_reaches_is_its_own_root_rather_than_a_panic() {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let root = scratch.path();
    // No `Cargo.toml` anywhere: nothing is a crate, and the scan still
    // produces records instead of failing.
    write(root, "loose.rs", "mod other;\nuse std::io;\n");
    write(root, "other.rs", "pub fn f() {}\n");
    let db = root.join("graph.redb");
    let report = scan_rust(root, &db).expect("a manifest-less tree still scans");
    let tally = report
        .per_lang
        .get(&Lang::Rust.code())
        .cloned()
        .unwrap_or_default();
    assert_eq!(tally.external, 1, "`std` still leaves the repository");
    // `mod other;` names a module `other.rs` would declare under `loose.rs`,
    // which it does not: each file is its own root when nothing reaches it.
    // Asserted as the outcome, not as a sum — a sum of one holds whether the
    // reference resolved or missed, which is the one thing this test is for.
    assert_eq!(tally.resolved, 0, "`mod other;` reaches no module");
    assert_eq!(
        rows(&db).get(&("loose.rs".to_string(), "mod other".to_string())),
        Some(&"unresolved:ModuleNotFound".to_string()),
    );
}

#[test]
fn a_dependency_inherited_from_the_workspace_root_still_points_into_the_repository() {
    // `foo = { workspace = true }` has been the ordinary way to name a
    // sibling crate since Cargo 1.64, and it states the `path` nowhere but in
    // the workspace root's `[workspace.dependencies]`. Reading only the
    // member's own table leaves no `path` to find, and the crate is filed
    // `External` — outside *both* terms of the resolution rate, so the
    // reference leaves the measurement rather than failing in it. Written as
    // a pair, because the inherited spelling and the inline one describe the
    // same repository and may not disagree.
    for inherited in [false, true] {
        let scratch = tempfile::tempdir().expect("scratch dir");
        let root = scratch.path();
        let (root_manifest, member_manifest) = if inherited {
            (
                "[workspace]\nmembers = [\"crates/one\", \"crates/two\"]\n\n\
                 [workspace.dependencies]\nlib-one = { path = \"crates/one\" }\n",
                "[package]\nname = \"lib-two\"\nedition = \"2024\"\n\n\
                 [dependencies]\nlib-one = { workspace = true }\n",
            )
        } else {
            (
                "[workspace]\nmembers = [\"crates/one\", \"crates/two\"]\n",
                "[package]\nname = \"lib-two\"\nedition = \"2024\"\n\n\
                 [dependencies]\nlib-one = { path = \"../one\" }\n",
            )
        };
        write(root, "Cargo.toml", root_manifest);
        write(
            root,
            "crates/one/Cargo.toml",
            "[package]\nname = \"lib-one\"\nedition = \"2024\"\n",
        );
        write(root, "crates/one/src/lib.rs", "pub struct Exported;\n");
        write(root, "crates/two/Cargo.toml", member_manifest);
        write(root, "crates/two/src/lib.rs", "use lib_one::Exported;\n");

        let db = root.join("graph.redb");
        let report = scan_rust(root, &db).expect("the workspace scans");
        let tally = report
            .per_lang
            .get(&Lang::Rust.code())
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            rows(&db).get(&(
                "crates/two/src/lib.rs".to_string(),
                "lib_one::Exported".to_string()
            )),
            Some(&"crates/one/src/lib.rs#Exported".to_string()),
            "inherited = {inherited}",
        );
        assert_eq!(tally.external, 0, "inherited = {inherited}");
    }
}

#[test]
fn a_module_written_beside_a_dependency_of_the_same_name_wins_the_way_rustc_gives_it() {
    // A crate name reaches a `use` path through the extern prelude, and a
    // prelude loses to a declaration written in the module. So `mod lib_one;`
    // beside a `path = …` dependency keyed `lib_one` binds the *module*, and
    // taking the dependency would be a wrong edge counted `Resolved` — worse
    // than a miss, because a wrong edge still reads as success.
    let scratch = tempfile::tempdir().expect("scratch dir");
    let root = scratch.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"app\"\nedition = \"2024\"\n\n\
         [[bin]]\nname = \"app\"\npath = \"src/main.rs\"\n\n\
         [dependencies]\nlib_one = { path = \"crates/one\" }\nserde = \"1\"\n",
    );
    write(
        root,
        "crates/one/Cargo.toml",
        "[package]\nname = \"lib_one\"\nedition = \"2024\"\n",
    );
    write(root, "crates/one/src/lib.rs", "pub struct Local;\n");
    write(root, "src/lib_one.rs", "pub struct Local;\n");
    write(root, "src/serde.rs", "pub struct Thing;\n");
    write(
        root,
        "src/main.rs",
        "mod lib_one;\nmod serde;\n\
         use lib_one::Local;\nuse serde::Thing;\nfn main() {}\n",
    );

    let db = root.join("graph.redb");
    scan_rust(root, &db).expect("the workspace scans");
    let rows = rows(&db);
    let got = |raw: &str| {
        rows.get(&("src/main.rs".to_string(), raw.to_string()))
            .unwrap_or_else(|| panic!("no row for `{raw}`; rows: {rows:#?}"))
            .as_str()
    };
    // The in-repository dependency, and the registry one, both lose to the
    // module written beside them.
    assert_eq!(got("lib_one::Local"), "src/main.rs::lib_one#Local");
    assert_eq!(got("serde::Thing"), "src/main.rs::serde#Thing");
}
