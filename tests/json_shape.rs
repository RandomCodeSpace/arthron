//! The JSON contract, pinned.
//!
//! Every test here compares a **whole document** against a literal, not a
//! handful of keys it happens to care about. That is the point: the field
//! names are public API from 0.0.1, and a rename, a dropped key, or an extra
//! one nobody meant to add has to fail the build rather than quietly empty
//! somebody's dashboard. When one of these tests fails, the question is
//! whether the change is a deliberate contract change — and if it is, whether
//! `arthron::json::SCHEMA` and the `--help` text moved with it.
//!
//! The scan and gate documents are built from hand-written tallies so the
//! numbers in the literals are arithmetic anyone can check. The query
//! documents come from a real scanned fixture, because a `DeclSite` is
//! something the store produces and inventing one would pin a shape the store
//! cannot actually emit.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use arthron::config::Config;
use arthron::gate::{Counts, GateFailure, GateVerdict, evaluate};
use arthron::json::{self, GateOutput};
use arthron::model::Lang;
use arthron::pipeline::scan_repo;
use arthron::query::{NameIndex, definition, impact, references};
use arthron::store::{LangTally, ReadStore, Report};

/// A report with one language's tally in it: 3 resolved out of a denominator
/// of 4, so the rate is exactly 0.75 and the literal below carries no rounding.
fn report() -> Report {
    Report {
        per_lang: BTreeMap::from([(
            Lang::Go.code(),
            LangTally {
                resolved: 3,
                external: 2,
                local_binding: 7,
                unresolved: BTreeMap::from([(5, 1)]),
            },
        )]),
        fqn_collisions: 1,
    }
}

/// The `config` sub-document a repository with no `arthron.toml` produces.
///
/// All three keys are present and empty. "No settings" and "this build does
/// not report settings" have to look different, because the second is what a
/// baseline compared under different globs would look like.
fn no_settings() -> Value {
    json!({ "include": [], "exclude": [], "tracks": {} })
}

/// The `languages` sub-document [`report`] produces, quoted once and shared by
/// the scan and gate literals — they are one contract, not two.
fn languages() -> Value {
    json!({
        "go": {
            "resolved": 3,
            "external": 2,
            "local_binding": 7,
            "unresolved": 1,
            "unresolved_reasons": { "NeedsTypeInference": 1 },
            "rate": 0.75,
        }
    })
}

#[test]
fn the_scan_document_is_exactly_this() {
    assert_eq!(
        json::scan(&report(), &Config::default()),
        json!({
            "schema": 1,
            "command": "scan",
            "config": no_settings(),
            "languages": languages(),
            "fqn_collisions": 1,
        }),
    );
}

#[test]
fn a_language_with_no_rows_has_no_entry() {
    // Not an all-zero tally: a rate of zero and the absence of any reference
    // are different facts, and the text report's unconditional Go line is a
    // courtesy to a human reader that machine output must not turn into a
    // measurement nobody took.
    let empty = Report::default();
    assert_eq!(
        json::scan(&empty, &Config::default()),
        json!({
            "schema": 1,
            "command": "scan",
            "config": no_settings(),
            "languages": {},
            "fqn_collisions": 0,
        }),
    );
}

/// The counts a gate compares in the tests below.
fn baseline_counts() -> Counts {
    Counts {
        resolved: 3,
        external: 2,
        local_binding: 7,
        unresolved: 1,
    }
}

fn gate_output<'a>(
    report: &'a Report,
    config: &'a Config,
    baseline: Counts,
    measured: Counts,
    verdict: Option<&'a GateVerdict>,
) -> GateOutput<'a> {
    GateOutput {
        language: "go",
        baseline_path: "baselines/go-codeiq.toml",
        corpus: "corpus/go/codeiq",
        commit: "deadbeef",
        config,
        report,
        baseline,
        measured,
        verdict,
    }
}

#[test]
fn the_passing_gate_document_is_exactly_this() {
    let report = report();
    let config = Config::default();
    let counts = baseline_counts();
    let verdict = GateVerdict::Pass { improved: false };
    assert_eq!(
        json::gate(&gate_output(
            &report,
            &config,
            counts,
            counts,
            Some(&verdict)
        )),
        json!({
            "schema": 1,
            "command": "gate",
            "action": "compare",
            "language": "go",
            "baseline_path": "baselines/go-codeiq.toml",
            "corpus": "corpus/go/codeiq",
            "commit": "deadbeef",
            "config": no_settings(),
            "verdict": "pass",
            "improved": false,
            "failures": [],
            "error": Value::Null,
            "baseline": {
                "resolved": 3, "external": 2, "local_binding": 7, "unresolved": 1,
            },
            "measured": {
                "resolved": 3, "external": 2, "local_binding": 7, "unresolved": 1,
            },
            "languages": languages(),
            "fqn_collisions": 1,
        }),
    );
}

#[test]
fn a_failing_gate_names_every_check_that_failed() {
    let report = report();
    let config = Config::default();
    let was = baseline_counts();
    // One run that regressed the rate *and* drifted both excluded buckets:
    // three failures, and all three are in the document.
    let now = Counts {
        resolved: 1,
        external: 5,
        local_binding: 9,
        unresolved: 3,
    };
    let verdict = evaluate(
        &arthron::gate::Baseline {
            format: arthron::gate::FORMAT,
            corpus: "corpus/go/codeiq".to_string(),
            commit: "deadbeef".to_string(),
            language: "go".to_string(),
            counts: was,
        },
        &now,
    );
    let doc = json::gate(&gate_output(&report, &config, was, now, Some(&verdict)));

    assert_eq!(doc["verdict"], json!("fail"));
    assert_eq!(doc["action"], json!("compare"));
    assert_eq!(doc["improved"], json!(false));
    assert_eq!(doc["error"], Value::Null);
    assert_eq!(
        doc["measured"],
        json!({ "resolved": 1, "external": 5, "local_binding": 9, "unresolved": 3 }),
    );
    let checks: Vec<&Value> = doc["failures"]
        .as_array()
        .expect("failures is an array")
        .iter()
        .map(|f| &f["check"])
        .collect();
    assert_eq!(
        checks,
        [
            &json!("rate_regressed"),
            &json!("local_binding_drift"),
            &json!("external_drift"),
        ],
    );
    // Every failure carries the sentence a person reads, beside the name a
    // script branches on.
    for failure in doc["failures"].as_array().expect("array") {
        assert!(
            failure["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{failure}",
        );
        assert_eq!(
            failure.as_object().expect("object").keys().len(),
            2,
            "a failure is exactly {{check, message}}: {failure}",
        );
    }
}

#[test]
fn a_rebase_says_it_rebased_and_reports_what_it_wrote() {
    let report = report();
    let config = Config::default();
    let measured = baseline_counts();
    let doc = json::gate(&gate_output(&report, &config, measured, measured, None));
    assert_eq!(doc["action"], json!("rebase"));
    assert_eq!(doc["verdict"], json!("rebased"));
    assert_eq!(doc["failures"], json!([]));
    assert_eq!(doc["error"], Value::Null);
    // The baseline side of a re-base is what was just written.
    assert_eq!(doc["baseline"], doc["measured"]);
}

#[test]
fn a_comparison_that_could_not_be_made_is_neither_a_pass_nor_a_failure() {
    let report = report();
    let verdict = GateVerdict::Error("baseline has nothing to measure".to_string());
    let doc = json::gate(&gate_output(
        &report,
        &Config::default(),
        Counts::default(),
        baseline_counts(),
        Some(&verdict),
    ));
    assert_eq!(doc["verdict"], json!("error"));
    assert_eq!(doc["error"], json!("baseline has nothing to measure"));
    assert_eq!(doc["failures"], json!([]));
}

#[test]
fn every_gate_document_carries_the_same_keys_whatever_it_decided() {
    let report = report();
    let config = Config::default();
    let counts = baseline_counts();
    let pass = GateVerdict::Pass { improved: true };
    let fail = GateVerdict::Fail(vec![GateFailure::ExternalDrift { was: 1, now: 2 }]);
    let error = GateVerdict::Error("nothing to measure".to_string());
    let mut key_sets = Vec::new();
    for verdict in [Some(&pass), Some(&fail), Some(&error), None] {
        let doc = json::gate(&gate_output(&report, &config, counts, counts, verdict));
        let keys: Vec<String> = doc.as_object().expect("object").keys().cloned().collect();
        key_sets.push(keys);
    }
    for keys in &key_sets[1..] {
        assert_eq!(
            keys, &key_sets[0],
            "a reader must not have to know the verdict to know the keys",
        );
    }
    assert_eq!(
        key_sets[0],
        [
            "action",
            "baseline",
            "baseline_path",
            "command",
            "commit",
            "config",
            "corpus",
            "error",
            "failures",
            "fqn_collisions",
            "improved",
            "language",
            "languages",
            "measured",
            "schema",
            "verdict",
        ],
    );
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// `api#Handle → server#Serve → util#Parse`: the same two-hop fixture the
/// query tests use, small enough that a whole document fits in a literal.
fn scanned(dir: &Path) -> ReadStore {
    write(dir, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        dir,
        "util/util.go",
        "package util\n\nfunc Parse(s string) string { return s }\n",
    );
    write(
        dir,
        "server/server.go",
        concat!(
            "package server\n\n",
            "import \"example.com/app/util\"\n\n",
            "func Serve() {\n",
            "\tutil.Parse(\"x\")\n",
            "}\n",
        ),
    );
    write(
        dir,
        "api/api.go",
        concat!(
            "package api\n\n",
            "import \"example.com/app/server\"\n\n",
            "func Handle() {\n",
            "\tserver.Serve()\n",
            "}\n",
        ),
    );
    let db = dir.join("graph.redb");
    scan_repo(dir, &db).expect("the fixture scans");
    ReadStore::open(&db).expect("the store opens read-only")
}

/// The one node a name selects, or a panic naming what it selected instead.
fn one(index: &NameIndex, name: &str) -> arthron::query::Match {
    let mut hits = index.lookup(name).matches;
    assert_eq!(hits.len(), 1, "{name} selected {hits:?}");
    hits.remove(0)
}

#[test]
fn the_query_def_document_is_exactly_this() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());
    let index = NameIndex::build(&store).expect("index builds");
    let node = one(&index, "Parse");
    let def = definition(&store, &node.id)
        .expect("the node reads")
        .expect("the node is there");

    assert_eq!(
        json::query_definition("Parse", &def, &[]),
        json!({
            "schema": 1,
            "command": "query",
            "verb": "def",
            "query": "Parse",
            "status": "ok",
            "shadowed": [],
            "fqn": "example.com/app/util#Parse",
            "kind": "function",
            "declarations": [{ "file": "util/util.go", "line": 3 }],
            "aliases": [],
        }),
    );
}

#[test]
fn the_query_refs_document_is_exactly_this() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());
    let index = NameIndex::build(&store).expect("index builds");
    let node = one(&index, "Parse");
    let sites = references(&store, &node.id).expect("the rows read");

    assert_eq!(
        json::query_references("Parse", &node, &sites, &[]),
        json!({
            "schema": 1,
            "command": "query",
            "verb": "refs",
            "query": "Parse",
            "status": "ok",
            "shadowed": [],
            "fqn": "example.com/app/util#Parse",
            "kind": "function",
            "rows": 1,
            "occurrences": 1,
            "references": [{
                "file": "server/server.go",
                "line": 6,
                "kind": "call",
                "enclosing": "example.com/app/server#Serve",
                "raw_target": "util.Parse",
                "count": 1,
                "language": "go",
                "outcome": {
                    "status": "resolved",
                    "package": Value::Null,
                    "reason": Value::Null,
                },
            }],
        }),
    );
}

#[test]
fn the_query_impact_document_is_exactly_this() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());
    let index = NameIndex::build(&store).expect("index builds");
    let node = one(&index, "Parse");
    let found = impact(&store, &node.id, 3).expect("the closure walks");

    assert_eq!(
        json::query_impact("Parse", &node, 3, &found, &[]),
        json!({
            "schema": 1,
            "command": "query",
            "verb": "impact",
            "query": "Parse",
            "status": "ok",
            "shadowed": [],
            "fqn": "example.com/app/util#Parse",
            "kind": "function",
            "depth": 3,
            "total": 2,
            "truncated": false,
            "layers": [
                { "depth": 1, "nodes": [
                    { "fqn": "example.com/app/server#Serve", "kind": "function" },
                ]},
                { "depth": 2, "nodes": [
                    { "fqn": "example.com/app/api#Handle", "kind": "function" },
                ]},
            ],
        }),
    );
}

#[test]
fn the_no_match_document_is_exactly_this() {
    assert_eq!(
        json::query_no_match("def", "NoSuchThing"),
        json!({
            "schema": 1,
            "command": "query",
            "verb": "def",
            "query": "NoSuchThing",
            "status": "no_match",
            "matches": [],
        }),
    );
}

#[test]
fn the_ambiguous_document_lists_every_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());
    write(
        dir.path(),
        "text/text.go",
        "package text\n\nfunc Parse(s string) string { return s }\n",
    );
    drop(store);
    let db = dir.path().join("graph.redb");
    scan_repo(dir.path(), &db).expect("the second package scans");
    let store = ReadStore::open(&db).expect("the store opens read-only");
    let index = NameIndex::build(&store).expect("index builds");
    let hits = index.lookup("Parse").matches;
    assert_eq!(hits.len(), 2, "{hits:?}");

    assert_eq!(
        json::query_ambiguous("def", "Parse", &hits),
        json!({
            "schema": 1,
            "command": "query",
            "verb": "def",
            "query": "Parse",
            "status": "ambiguous",
            "matches": [
                { "fqn": "example.com/app/text#Parse", "kind": "function" },
                { "fqn": "example.com/app/util#Parse", "kind": "function" },
            ],
        }),
    );
}

#[test]
fn a_document_renders_the_same_way_twice() {
    // Key order is the serializer's, not a hash map's: two runs over one store
    // must print byte-identical documents or a diff in CI is noise.
    let doc = json::scan(&report(), &Config::default());
    let once = json::render(&doc).expect("renders");
    let twice = json::render(&doc).expect("renders");
    assert_eq!(once, twice);
    assert!(once.starts_with('{'), "{once}");
}

/// Every top-level key of a document, sorted.
fn keys(doc: &Value) -> Vec<String> {
    doc.as_object()
        .expect("a document is an object")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn every_field_a_document_carries_is_named_in_the_help() {
    // "Design field names once, document them in --help" — enforced, because
    // a contract documented only in the source is a contract nobody can read.
    let report = report();
    let counts = baseline_counts();
    let verdict = GateVerdict::Pass { improved: false };
    let dir = tempfile::tempdir().unwrap();
    let store = scanned(dir.path());
    let index = NameIndex::build(&store).expect("index builds");
    let node = one(&index, "Parse");
    let def = definition(&store, &node.id)
        .expect("the node reads")
        .expect("the node is there");
    let sites = references(&store, &node.id).expect("the rows read");
    let found = impact(&store, &node.id, 3).expect("the closure walks");

    let config = Config::default();
    let documents = [
        json::scan(&report, &config),
        json::gate(&gate_output(
            &report,
            &config,
            counts,
            counts,
            Some(&verdict),
        )),
        json::query_definition("Parse", &def, &[]),
        json::query_references("Parse", &node, &sites, &[]),
        json::query_impact("Parse", &node, 3, &found, &[]),
        json::query_no_match("def", "Nothing"),
        json::query_ambiguous("def", "Parse", std::slice::from_ref(&node)),
    ];
    for doc in &documents {
        for key in keys(doc) {
            assert!(
                json::HELP.contains(&key),
                "`{key}` is emitted but nowhere in --help",
            );
        }
    }

    // …and the nested keys a reader has to know to walk a row.
    for key in [
        "resolved",
        "external",
        "local_binding",
        "unresolved",
        "unresolved_reasons",
        "rate",
        "check",
        "message",
        "file",
        "line",
        "raw_target",
        "enclosing",
        "count",
        "package",
        "reason",
        "nodes",
    ] {
        assert!(
            json::HELP.contains(key),
            "the nested field `{key}` is emitted but nowhere in --help",
        );
    }
}
