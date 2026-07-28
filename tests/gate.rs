//! End-to-end for the `gate` subcommand: record a baseline from a real scan,
//! then gate against it.
//!
//! This drives the built binary rather than the library, because the exit
//! code *is* the gate — a verdict a CI job cannot read is not a gate. The
//! committed corpus baselines are deliberately not exercised here: a test
//! that re-derived them would only restate the file.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use arthron::registry::REGISTRY;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A two-package module with a known mix of every outcome column: resolved,
/// external, local-binding and unresolved.
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
            "import (\n\t\"fmt\"\n\t\"example.com/app/util\"\n)\n\n",
            "var pool Conn\n\n",
            "func Serve(conn Conn) {\n",
            "\tfmt.Println(util.Parse(\"x\"))\n",
            "\thelper()\n",
            "\tmissing()\n",
            "\tconn.Close()\n",
            "\tpool.Close()\n",
            "}\n\n",
            "func helper() {}\n\n",
            "type Conn struct{}\n",
        ),
    );
}

fn gate(root: &Path, baseline: &Path, db: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arthron"))
        .arg("gate")
        .arg(root)
        .arg("--baseline")
        .arg(baseline)
        .arg("--db")
        .arg(db)
        .args(extra)
        .output()
        .expect("running the arthron binary")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("the process exited normally")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn a_rebased_baseline_gates_the_scan_it_was_recorded_from() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    fixture(&root);
    let baseline = dir.path().join("go-fixture.toml");

    let recorded = gate(
        &root,
        &baseline,
        &dir.path().join("rebase.redb"),
        &["--rebase", "--commit", "0000000"],
    );
    assert_eq!(code(&recorded), 0, "{}", stderr(&recorded));

    let text = fs::read_to_string(&baseline).expect("the baseline was written");
    let parsed = arthron::gate::parse_baseline(&text).expect("it reads back");
    assert_eq!(parsed.language, "go");
    assert_eq!(parsed.commit, "0000000");
    // The counts are the fixture's, measured — not a guess written into a
    // file that would then gate every later run.
    //
    // Two of the five resolved are type uses: `Conn` in `var pool Conn` and
    // in `func Serve(conn Conn)`. Two of the four external are the `string`
    // in `Parse`'s signature and its result, which are Go universe names;
    // `fmt` and the `fmt.Println` call are the other two.
    assert_eq!(parsed.counts.resolved, 5);
    assert_eq!(parsed.counts.external, 4);
    assert_eq!(parsed.counts.local_binding, 1);
    assert_eq!(parsed.counts.unresolved, 2);

    // A second run, cold store, same tree: pass.
    let passed = gate(&root, &baseline, &dir.path().join("gate.redb"), &[]);
    assert_eq!(code(&passed), 0, "{}", stderr(&passed));
}

#[test]
fn a_baseline_claiming_a_higher_rate_fails_the_gate() {
    // The regression path, driven the way CI drives it: the exit code, not
    // the prose.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    fixture(&root);
    let baseline = dir.path().join("go-fixture.toml");

    let recorded = gate(
        &root,
        &baseline,
        &dir.path().join("rebase.redb"),
        &["--rebase", "--commit", "0000000"],
    );
    assert_eq!(code(&recorded), 0, "{}", stderr(&recorded));

    let text = fs::read_to_string(&baseline).unwrap();
    fs::write(&baseline, text.replace("resolved = 5", "resolved = 6")).unwrap();

    let failed = gate(&root, &baseline, &dir.path().join("gate.redb"), &[]);
    assert_eq!(code(&failed), 1, "{}", stderr(&failed));
    assert!(
        stderr(&failed).contains("resolution rate regressed"),
        "{}",
        stderr(&failed),
    );
}

#[test]
fn a_malformed_baseline_is_a_usage_error_not_a_pass() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    fixture(&root);
    let baseline = dir.path().join("go-fixture.toml");
    fs::write(&baseline, "[counts]\nresolved = 3\n").unwrap();

    let broken = gate(&root, &baseline, &dir.path().join("gate.redb"), &[]);
    assert_eq!(code(&broken), 2, "{}", stderr(&broken));
    assert!(
        stderr(&broken).contains("table headers"),
        "{}",
        stderr(&broken)
    );
}

#[test]
fn gating_against_a_baseline_that_does_not_exist_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    fixture(&root);

    let missing = gate(
        &root,
        &dir.path().join("absent.toml"),
        &dir.path().join("gate.redb"),
        &[],
    );
    assert_eq!(code(&missing), 2, "{}", stderr(&missing));
    assert!(
        stderr(&missing).contains("--rebase"),
        "{}",
        stderr(&missing)
    );
}

#[test]
fn a_registered_but_disabled_language_is_refused_before_the_scan() {
    // `Lang::ALL` carries every ratified language, most of whose tracks are
    // not live. Gating one of those can only ever fail, so it must fail
    // immediately: a name that validated and then scanned would spend a
    // whole cold run on a corpus before reporting a usage error.
    //
    // The language is read off the registry rather than written here, so a
    // track going live retires it from this test instead of breaking it —
    // which is the whole point of the registry's zero-conflict rule, and is
    // exactly what caught this file when Rust went live.
    let Some(disabled) = REGISTRY
        .iter()
        .filter(|t| !t.is_enabled())
        .flat_map(|t| t.langs)
        .map(|l| l.name())
        .next()
    else {
        println!("SKIP: every registered track is live, so nothing can be refused");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    fixture(&root);
    let db = dir.path().join("never.redb");

    let out = gate(
        &root,
        &dir.path().join("b.toml"),
        &db,
        &["--language", disabled, "--rebase"],
    );
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("not live"), "stderr: {err}");
    assert!(
        err.contains(disabled),
        "the error names the language: {err}"
    );
    assert!(
        err.contains("go"),
        "the error names what can be gated: {err}"
    );
    // Nothing was measured: no report on stdout and no store on disk.
    let out_text = String::from_utf8_lossy(&out.stdout);
    assert!(out_text.is_empty(), "a scan ran anyway: {out_text}");
    assert!(!db.exists(), "a scan ran anyway: {} exists", db.display());
}

#[test]
fn an_unknown_language_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = gate(
        dir.path(),
        &dir.path().join("b.toml"),
        &dir.path().join("g.redb"),
        &["--language", "klingon"],
    );
    assert_eq!(out.status.code(), Some(2), "usage error is exit 2");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown language"), "stderr: {err}");
    assert!(
        err.contains("go"),
        "the error names the valid languages: {err}"
    );
}
