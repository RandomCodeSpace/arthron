//! The registry seam: what a scan reads is decided by the registry, and a
//! disabled track decides nothing.
//!
//! These are end-to-end because the interesting claim is not what
//! [`arthron::registry::Track::owns_extension`] returns — the unit tests
//! cover that — but that a repository full of Java, JavaScript, TypeScript
//! and Python still produces exactly the graph the Go-only scan produced.
//! A track that leaked would show up here as a node, a row, or a report line.

use std::fs;
use std::path::Path;

use arthron::model::Lang;
use arthron::pipeline::{scan_go, scan_repo};
use arthron::registry::REGISTRY;
use arthron::store::Store;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A module with one real Go package and one file per not-yet-live track,
/// each naming things a live extractor would certainly have picked up.
fn mixed_tree(root: &Path) {
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "app/app.go",
        concat!(
            "package app\n\n",
            "func Run() { helper() }\n\n",
            "func helper() {}\n",
        ),
    );
    write(
        root,
        "app/Greeter.java",
        "package app;\n\nclass Greeter { void hi() { hi(); } }\n",
    );
    write(root, "app/greet.js", "export function hi() { hi(); }\n");
    write(root, "app/greet.mjs", "export function hi() { hi(); }\n");
    write(root, "app/greet.cjs", "function hi() { hi(); }\n");
    write(
        root,
        "app/greet.ts",
        "export function hi(): void { hi(); }\n",
    );
    write(
        root,
        "app/types.d.ts",
        "export declare function hi(): void;\n",
    );
    write(root, "app/greet.py", "def hi():\n    hi()\n");
}

#[test]
fn registry_iteration_order_is_deterministic() {
    let once: Vec<&str> = REGISTRY.iter().map(|t| t.name).collect();
    let twice: Vec<&str> = REGISTRY.iter().map(|t| t.name).collect();
    assert_eq!(once, twice);
    assert_eq!(once, ["go", "java", "ecma", "python"]);

    // Every language is registered to exactly one track, so "which track
    // reads this file" has one answer rather than a precedence rule.
    for lang in Lang::ALL {
        let owners: Vec<&str> = REGISTRY
            .iter()
            .filter(|t| t.langs.contains(lang))
            .map(|t| t.name)
            .collect();
        assert_eq!(owners.len(), 1, "{} is registered {owners:?}", lang.name());
    }
}

#[test]
fn a_disabled_track_owns_no_file() {
    for track in REGISTRY.iter().filter(|t| !t.is_enabled()) {
        for lang in track.langs {
            for ext in lang.extensions() {
                assert!(
                    !track.owns_extension(ext),
                    "disabled track `{}` claims `.{ext}`",
                    track.name,
                );
            }
        }
    }
    // And the converse, so this test cannot pass by the registry being empty.
    let live: Vec<&str> = REGISTRY
        .iter()
        .filter(|t| t.is_enabled())
        .map(|t| t.name)
        .collect();
    assert_eq!(live, ["go"]);
}

#[test]
fn scanning_a_mixed_language_tree_scans_only_go() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mixed_tree(root);
    let db = root.join("graph.redb");

    let report = scan_repo(root, &db).expect("scan succeeds");

    // One report line, and it is Go's. Not one line per registered language:
    // a track that read nothing has nothing to report, and inventing a zero
    // row for it would make "not built" look like "measured, found nothing".
    assert_eq!(
        report.per_lang.keys().copied().collect::<Vec<_>>(),
        [Lang::Go.code()]
    );
    let go = &report.per_lang[&Lang::Go.code()];
    assert!(go.resolved > 0, "the Go half of the tree still resolves");

    // No file from a disabled track was read. `Store::known_files` is every
    // file any scan wrote a half for, so a leaked read shows up here whatever
    // it did or did not extract.
    let store = Store::open(&db).expect("store opens");
    let files = store.known_files().expect("known files");
    assert_eq!(files, ["app/app.go"]);
    for file in &files {
        let ext = Path::new(file).extension().and_then(|e| e.to_str());
        assert_eq!(ext, Some("go"), "{file} is not a Go file");
    }
}

#[test]
fn the_registry_scan_and_the_go_scan_agree_while_go_is_the_only_track() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mixed_tree(root);

    // Two cold stores, same tree: going through the registry must not change
    // one byte of what a direct Go scan produces.
    let via_registry = scan_repo(root, &root.join("registry.redb")).expect("registry scan");
    let via_go = scan_go(root, &root.join("go.redb")).expect("go scan");
    assert_eq!(via_registry, via_go);
}
