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

/// A module with one real Go package plus a file for each of the other live
/// tracks and for three registered-but-disabled ones, each naming things a
/// live extractor would certainly have picked up.
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
    // Three of the registered-but-disabled tier-2 tracks, so that "a disabled
    // track owns no file" is asserted against files that exist rather than
    // against an empty tree.
    write(root, "app/greet.rs", "pub fn hi() { hi(); }\n");
    write(root, "app/greet.rb", "def hi\n  hi\nend\n");
    write(root, "app/greet.cpp", "void hi() { hi(); }\n");
}

#[test]
fn registry_iteration_order_is_deterministic() {
    let once: Vec<&str> = REGISTRY.iter().map(|t| t.name).collect();
    let twice: Vec<&str> = REGISTRY.iter().map(|t| t.name).collect();
    assert_eq!(once, twice);
    assert_eq!(
        once,
        [
            "go", "java", "ecma", "python", "cpp", "csharp", "kotlin", "swift", "ruby", "php",
            "rust", "scala", "dart", "elixir", "haskell", "lua", "bash", "hcl",
        ]
    );

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
    // Which tracks beyond Go are live changes as tracks land; Go always is.
    let live: Vec<&str> = REGISTRY
        .iter()
        .filter(|t| t.is_enabled())
        .map(|t| t.name)
        .collect();
    assert!(live.contains(&"go"), "go is not live: {live:?}");
}

/// The track that owns `file`'s extension, if any language claims it.
fn owning_track(file: &str) -> Option<&'static arthron::registry::Track> {
    let ext = Path::new(file).extension().and_then(|e| e.to_str())?;
    REGISTRY
        .iter()
        .find(|t| t.langs.iter().any(|l| l.extensions().contains(&ext)))
}

#[test]
fn scanning_a_mixed_language_tree_reads_only_live_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mixed_tree(root);
    let db = root.join("graph.redb");

    let report = scan_repo(root, &db).expect("scan succeeds");

    // Every report line belongs to a live track — never one line per
    // registered language: a track that read nothing has nothing to report,
    // and inventing a zero row for it would make "not built" look like
    // "measured, found nothing". Go's line is always among them.
    for code in report.per_lang.keys() {
        let lang = Lang::from_code(*code).expect("reported lang exists");
        let track = REGISTRY
            .iter()
            .find(|t| t.langs.contains(&lang))
            .expect("reported lang is registered");
        assert!(
            track.is_enabled(),
            "disabled track `{}` produced a report line",
            track.name,
        );
    }
    let go = &report.per_lang[&Lang::Go.code()];
    assert!(go.resolved > 0, "the Go half of the tree still resolves");

    // No file from a disabled track was read. `Store::known_files` is every
    // file any scan wrote a half for, so a leaked read shows up here whatever
    // it did or did not extract.
    let store = Store::open(&db).expect("store opens");
    let files = store.known_files().expect("known files");
    assert!(
        files.contains(&"app/app.go".to_string()),
        "the Go file was not scanned: {files:?}",
    );
    for file in &files {
        let track = owning_track(file).expect("scanned file has an owner");
        assert!(
            track.is_enabled(),
            "disabled track `{}` leaked a read of {file}",
            track.name,
        );
    }
}

#[test]
fn the_registry_scan_never_changes_gos_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    mixed_tree(root);

    // Two cold stores, same tree: whatever other tracks are live, going
    // through the registry must not change one byte of Go's tally against
    // a direct Go-only scan.
    let via_registry = scan_repo(root, &root.join("registry.redb")).expect("registry scan");
    let via_go = scan_go(root, &root.join("go.redb")).expect("go scan");
    assert_eq!(
        via_registry.per_lang[&Lang::Go.code()],
        via_go.per_lang[&Lang::Go.code()],
    );
}
