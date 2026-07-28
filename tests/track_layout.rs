//! One track's missing manifest is one track's answer, never the whole scan's.
//!
//! Phase 0 asks each resolver for its project's layout, and a language whose
//! manifest is not in the tree cannot answer. That is a fact about *that*
//! language here — nothing else in the registry was ever asked about
//! `go.mod` — so it is reported for that language and every other track still
//! runs. Before this, one stray `.go` file in a JavaScript repository took
//! every other language's answer with it: exit 1, no report, nothing measured
//! at all.
//!
//! The other half is just as important and has its own test: a track that
//! measured nothing has to *say* so. Silently reporting no Go is the same
//! class of bug read from the other side, and the scan already has a channel
//! for a thing it reached and could not turn into facts.

use std::fs;
use std::path::Path;
use std::process::Command;

use arthron::model::Lang;
use arthron::pipeline::scan_repo;
use arthron::store::Report;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    fs::write(path, content).expect("write");
}

/// A JavaScript and Python repository with one stray Go file in a
/// subdirectory, and no `go.mod` anywhere in it.
///
/// Each language references within itself, so each has reference rows and
/// therefore a tally: a report keyed off references cannot show a language
/// that never referenced anything.
fn fixture(root: &Path) {
    write(
        root,
        "src/a.js",
        "import { b } from './b.js';\nexport function a() { return b(); }\n",
    );
    write(root, "src/b.js", "export function b() { return 1; }\n");
    write(
        root,
        "app.py",
        "import helper\n\n\ndef main():\n    return helper.parse()\n",
    );
    write(root, "helper.py", "def parse():\n    return 2\n");
    write(
        root,
        "tools/gen.go",
        "package tools\n\nfunc Gen() {}\n\nfunc use() { Gen() }\n",
    );
}

fn tallied(report: &Report) -> Vec<&'static str> {
    report
        .per_lang
        .keys()
        .filter_map(|code| Lang::from_code(*code).map(Lang::name))
        .collect()
}

fn errored(report: &Report, path: &str) -> Option<String> {
    report
        .file_errors
        .iter()
        .find(|e| e.path == path)
        .map(|e| e.message.clone())
}

#[test]
fn a_track_with_no_project_of_its_own_does_not_take_the_others_answers_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);

    let report = scan_repo(root, &root.join("graph.redb"))
        .expect("a tree with no go.mod is still a tree the other tracks can measure");
    let langs = tallied(&report);
    assert!(
        langs.contains(&"javascript"),
        "one track's precondition took JavaScript's answer with it: {langs:?}",
    );
    assert!(
        langs.contains(&"python"),
        "one track's precondition took Python's answer with it: {langs:?}",
    );
}

#[test]
fn the_track_that_found_no_project_says_so_rather_than_going_quiet() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);

    let report = scan_repo(root, &root.join("graph.redb")).expect("the scan answers");
    let said = errored(&report, Lang::Go.name())
        .unwrap_or_else(|| panic!("nothing said Go measured nothing: {:?}", report.file_errors));
    assert!(
        said.contains("go.mod"),
        "the entry has to say what was missing: {said}",
    );
    assert!(
        !tallied(&report).contains(&"go"),
        "a track with no project reported a tally anyway: {:?}",
        tallied(&report),
    );
}

#[test]
fn the_cli_answers_the_other_languages_and_exits_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);

    let out = Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args([
            "scan",
            root.to_str().expect("a utf-8 temp path"),
            "--db",
            root.join("graph.redb").to_str().expect("a utf-8 temp path"),
        ])
        .output()
        .expect("running the arthron binary");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a per-track precondition failed the whole scan: {stderr}",
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("javascript"), "{text}");
    assert!(text.contains("python"), "{text}");
    // …and the Go track's silence is on the record, in the same place every
    // other thing the scan reached and could not use is.
    assert!(text.contains("file errors"), "{text}");
    assert!(text.contains("go.mod"), "{text}");
}

#[test]
fn the_json_document_carries_the_track_that_found_no_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);

    let report = scan_repo(root, &root.join("graph.redb")).expect("the scan answers");
    let doc = arthron::json::scan(&report, &arthron::config::Config::default());
    let entries = doc["file_errors"]
        .as_array()
        .expect("file_errors is always an array")
        .clone();
    assert!(
        entries.iter().any(|e| e["path"] == Lang::Go.name()
            && e["error"].as_str().is_some_and(|m| m.contains("go.mod"))),
        "{entries:?}",
    );
}

/// The document must not disagree with itself over a store that outlives one
/// scan. `Store::report` counts rows, and rows survive a manifest that broke
/// after they were written — so the same document carried "go measured
/// nothing" beside the tally of the scan before it, and `gate --db` on that
/// store would re-base a baseline onto numbers no scan produced.
#[test]
fn a_layout_that_broke_since_the_last_scan_reports_no_tally_from_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let db = dir.path().join("kept.redb");
    fixture(root);
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");

    let first = scan_repo(root, &db).expect("the scan answers");
    assert!(
        tallied(&first).contains(&"go"),
        "the fixture has to measure Go for this test to mean anything: {:?}",
        tallied(&first),
    );

    // The manifest breaks; the `.go` files and the rows they wrote stay.
    write(root, "go.mod", "this is not a go.mod\n");
    let second = scan_repo(root, &db).expect("the scan answers");
    assert!(
        errored(&second, Lang::Go.name()).is_some(),
        "the track stopped saying it measured nothing: {:?}",
        second.file_errors,
    );
    assert!(
        !tallied(&second).contains(&"go"),
        "the report claimed a Go tally the run did not measure: {:?}",
        tallied(&second),
    );
    // The other tracks are untouched, and the rows are still there to be
    // measured again once the manifest is readable.
    assert!(
        tallied(&second).contains(&"python"),
        "suppressing one language took another's answer with it: {:?}",
        tallied(&second),
    );
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    let third = scan_repo(root, &db).expect("the scan answers");
    assert_eq!(
        third.per_lang.get(&Lang::Go.code()),
        first.per_lang.get(&Lang::Go.code()),
        "the rows were forgotten rather than left for the next readable scan",
    );
}

#[test]
fn a_manifest_that_is_there_is_untouched_by_any_of_this() {
    // The fix must not turn into "Go never runs": with a `go.mod` the track
    // measures exactly what it always did and says nothing about layout.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");

    let report = scan_repo(root, &root.join("graph.redb")).expect("the scan answers");
    assert!(tallied(&report).contains(&"go"), "{:?}", tallied(&report),);
    assert_eq!(
        errored(&report, Lang::Go.name()),
        None,
        "a track with a manifest still claimed it had none: {:?}",
        report.file_errors,
    );
}
