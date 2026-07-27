//! `--json` as the binary actually emits it.
//!
//! The document shapes are pinned in `tests/json_shape.rs`, against the
//! library. What is here is the part that only the process can show: that the
//! flag exists on all three commands, that stdout is a single parseable
//! document and nothing else, that the exit code still means what it meant,
//! and that a usage error stays a stderr line rather than becoming a document
//! a script would read as a result.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// `api#Handle → server#Serve → util#Parse`.
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

fn arthron(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args(args)
        .output()
        .expect("running the arthron binary")
}

/// Stdout parsed as one JSON document, plus the exit code.
///
/// Parsing the *whole* of stdout is the assertion: a stray report line beside
/// the document would make `jq` fail, so it has to make this fail.
fn json(out: &Output) -> (Value, i32) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let doc = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}):\n{stdout}"));
    (doc, out.status.code().expect("the process exited normally"))
}

fn scanned(root: &Path) -> std::path::PathBuf {
    fixture(root);
    let db = root.join("graph.redb");
    let out = arthron(&["scan", root.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    db
}

#[test]
fn scan_json_is_the_whole_of_stdout() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let db = dir.path().join("graph.redb");
    let out = arthron(&[
        "scan",
        dir.path().to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--json",
    ]);
    let (doc, code) = json(&out);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(doc["command"], "scan");
    assert_eq!(doc["schema"], 1);
    // Two imports and two calls, all four linked: the fixture's whole
    // reference set, and nothing in it fails to resolve.
    assert_eq!(doc["languages"]["go"]["resolved"], 4);
    assert_eq!(doc["languages"]["go"]["unresolved"], 0);
    assert_eq!(doc["languages"]["go"]["rate"], 1.0);
    assert_eq!(doc["fqn_collisions"], 0);
}

#[test]
fn every_query_verb_takes_the_flag_after_its_name() {
    // `--json` is global, like `--db`, so it may trail the verb — which is
    // where a person types it.
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());
    for (verb, name) in [("def", "Parse"), ("refs", "Parse"), ("impact", "Parse")] {
        let out = arthron(&["query", verb, name, "--db", db.to_str().unwrap(), "--json"]);
        let (doc, code) = json(&out);
        assert_eq!(code, 0, "{verb}: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(doc["command"], "query");
        assert_eq!(doc["verb"], verb);
        assert_eq!(doc["status"], "ok");
        assert_eq!(doc["fqn"], "example.com/app/util#Parse");
    }
}

#[test]
fn an_unanswered_query_is_a_document_and_still_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let out = arthron(&[
        "query",
        "def",
        "NoSuchThing",
        "--db",
        db.to_str().unwrap(),
        "--json",
    ]);
    let (doc, code) = json(&out);
    assert_eq!(code, 1, "no match is an answer, and not a zero one");
    assert_eq!(doc["status"], "no_match");
    assert_eq!(doc["matches"], serde_json::json!([]));
}

#[test]
fn an_ambiguous_query_lists_its_candidates_as_a_document() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    write(
        dir.path(),
        "text/text.go",
        "package text\n\nfunc Parse(s string) string { return s }\n",
    );
    let db = dir.path().join("graph.redb");
    assert!(
        arthron(&[
            "scan",
            dir.path().to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ])
        .status
        .success()
    );

    let out = arthron(&[
        "query",
        "def",
        "Parse",
        "--db",
        db.to_str().unwrap(),
        "--json",
    ]);
    let (doc, code) = json(&out);
    assert_eq!(code, 1);
    assert_eq!(doc["status"], "ambiguous");
    let matches = doc["matches"].as_array().expect("an array");
    assert_eq!(matches.len(), 2, "{matches:?}");
    // Every candidate is named, because picking one would be a guess.
    assert_eq!(matches[0]["fqn"], "example.com/app/text#Parse");
    assert_eq!(matches[1]["fqn"], "example.com/app/util#Parse");
}

#[test]
fn an_io_error_stays_a_stderr_line_and_never_becomes_a_document() {
    // Nothing was measured, so there is no result to hand a script — and a
    // script must never read an empty document as an empty answer.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("never-scanned.redb");
    let out = arthron(&[
        "query",
        "def",
        "Parse",
        "--db",
        db.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        out.stdout.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!out.stderr.is_empty());
}

#[test]
fn a_gate_verdict_is_a_document_and_the_exit_code_agrees_with_it() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let baseline = dir.path().join("go.toml");

    // Record a baseline: the re-base says so in the document it prints.
    let out = arthron(&[
        "gate",
        dir.path().to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--rebase",
        "--json",
    ]);
    let (doc, code) = json(&out);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(doc["action"], "rebase");
    assert_eq!(doc["verdict"], "rebased");
    assert_eq!(doc["language"], "go");
    assert_eq!(doc["measured"]["resolved"], 4);
    assert_eq!(doc["baseline"], doc["measured"]);

    // Comparing the same tree against it passes.
    let out = arthron(&[
        "gate",
        dir.path().to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--json",
    ]);
    let (doc, code) = json(&out);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(doc["action"], "compare");
    assert_eq!(doc["verdict"], "pass");
    assert_eq!(doc["improved"], false);
    assert_eq!(doc["failures"], serde_json::json!([]));
}

#[test]
fn a_failing_gate_prints_its_document_and_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let baseline = dir.path().join("go.toml");
    assert!(
        arthron(&[
            "gate",
            dir.path().to_str().unwrap(),
            "--baseline",
            baseline.to_str().unwrap(),
            "--rebase",
        ])
        .status
        .success()
    );

    // Break a call: `util.Parse` becomes a name nothing declares, so one
    // resolved reference turns into an unresolved one and the rate falls.
    write(
        dir.path(),
        "server/server.go",
        concat!(
            "package server\n\n",
            "import \"example.com/app/util\"\n\n",
            "func Serve() {\n",
            "\tutil.Missing(\"x\")\n",
            "}\n",
        ),
    );

    let out = arthron(&[
        "gate",
        dir.path().to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--json",
    ]);
    let (doc, code) = json(&out);
    assert_eq!(code, 1, "a regression is a failure, not an error");
    assert_eq!(doc["verdict"], "fail");
    let failures = doc["failures"].as_array().expect("an array");
    assert!(
        failures.iter().any(|f| f["check"] == "rate_regressed"),
        "{failures:?}",
    );
}

#[test]
fn the_help_documents_the_schema_and_the_config_file() {
    // The field names are a public contract, so they are written down where a
    // user looks — not only in the source.
    for command in ["scan", "gate", "query"] {
        let out = Command::new(env!("CARGO_BIN_EXE_arthron"))
            .args([command, "--help"])
            .output()
            .expect("running the binary");
        let help = String::from_utf8_lossy(&out.stdout);
        assert!(help.contains("--json"), "{command}: {help}");
        for field in [
            "unresolved_reasons",
            "fqn_collisions",
            "local_binding",
            "rate_regressed",
            "raw_target",
        ] {
            assert!(help.contains(field), "{command} --help omits `{field}`");
        }
    }

    // The configuration file is documented on the two commands that read a
    // repository, and nowhere it would be a lie.
    for command in ["scan", "gate"] {
        let out = Command::new(env!("CARGO_BIN_EXE_arthron"))
            .args([command, "--help"])
            .output()
            .expect("running the binary");
        let help = String::from_utf8_lossy(&out.stdout);
        assert!(help.contains("arthron.toml"), "{command}: {help}");
        assert!(help.contains("[tracks]"), "{command}: {help}");
    }
}

/// Every measurement document says which file set it was taken over.
///
/// `corpus` and `commit` are provenance for *where* a baseline came from;
/// `include`, `exclude` and `[tracks]` are provenance for *what was counted*,
/// and without them a baseline recorded under one configuration and compared
/// under another compares two different repositories with nothing to show it.
/// The dangerous shape is partial under-match: excluding the files a language
/// resolves worst makes the rate improve and the gate pass.
#[test]
fn scan_json_records_the_settings_the_counts_were_taken_under() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());
    write(
        dir.path(),
        "arthron.toml",
        "include = [\"util/**\", \"server/**\"]\nexclude = [\"**/vendor/**\"]\n\n[tracks]\njava = false\n",
    );

    let out = arthron(&[
        "scan",
        dir.path().to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--json",
    ]);
    let (doc, code) = json(&out);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        doc["config"],
        serde_json::json!({
            "include": ["util/**", "server/**"],
            "exclude": ["**/vendor/**"],
            "tracks": { "java": false },
        }),
        "{doc}",
    );
}

/// A repository with no configuration file still carries the keys, empty.
#[test]
fn a_repository_with_no_config_says_so_rather_than_omitting_it() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let db = dir.path().join("graph.redb");
    let out = arthron(&[
        "scan",
        dir.path().to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--json",
    ]);
    let (doc, code) = json(&out);
    assert_eq!(code, 0);
    assert_eq!(
        doc["config"],
        serde_json::json!({ "include": [], "exclude": [], "tracks": {} }),
        "{doc}",
    );
}

/// A whitelist that matches no file is said out loud.
///
/// `include = ["src"]` matches the directory and no file under it, so the scan
/// reads nothing and reports a clean run: rate `n/a`, exit 0, and
/// `"languages": {}` — the same document an empty repository produces. `gate`
/// already refuses a zero denominator; this is the `scan` half of that guard.
#[test]
fn an_include_that_matched_nothing_is_a_warning_and_not_a_clean_run() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    // A bare directory name: the trap, spelled exactly as somebody would.
    write(dir.path(), "arthron.toml", "include = [\"util\"]\n");
    let db = dir.path().join("graph.redb");

    let out = arthron(&[
        "scan",
        dir.path().to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--json",
    ]);
    let (doc, code) = json(&out);
    // Still an answer and still exit 0: measuring nothing is a legitimate
    // fact about an empty tree, and this cannot tell the two apart.
    assert_eq!(code, 0);
    assert_eq!(doc["languages"], serde_json::json!({}), "{doc}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("`include`"), "{stderr}");
    assert!(
        stderr.contains("src/**"),
        "the fix is in the message: {stderr}"
    );
    // And the document itself carries the globs, so a dashboard can tell
    // "your config is wrong" from "no data".
    assert_eq!(doc["config"]["include"], serde_json::json!(["util"]));
}

/// A whitelist that matches files says nothing.
#[test]
fn an_include_that_matched_files_warns_about_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    write(dir.path(), "arthron.toml", "include = [\"**/*.go\"]\n");
    let db = dir.path().join("graph.redb");

    let out = arthron(&[
        "scan",
        dir.path().to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("`include`"), "{stderr}");
}
