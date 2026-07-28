//! A run that answered is not undone by a stderr nobody can write to.
//!
//! `eprintln!` panics when the write fails, and a panic is exit 101 and a
//! backtrace — which no script can tell from a real crash. So a scan that
//! measured a tree, produced its report and then had one advisory line to add
//! died at 101 with an empty stdout, because the disk stderr pointed at was
//! full. The answer was right there and nobody got it.
//!
//! Stdout already had this rule — one write, and a closed reader is not this
//! program's failure. This is the same rule on the other stream, with the one
//! extra clause an error channel needs: a message that cannot be delivered
//! must still leave the exit code telling the truth.
//!
//! # Why Linux only
//!
//! `/dev/full` is how a write is made to fail on demand without filling a real
//! filesystem. It is a Linux device; the test is inexpressible without it
//! rather than merely skipped elsewhere.

#![cfg(target_os = "linux")]

use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A Go module whose `include` glob matches nothing, so a successful scan has
/// an advisory line to write to stderr.
///
/// A bare directory name matches the directory and not the files under it,
/// which is exactly the mistake the advisory exists to name.
fn fixture(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("mkdir");
    fs::write(root.join("go.mod"), "module example.com/app\n\ngo 1.22\n").expect("go.mod");
    fs::write(
        root.join("src/a.go"),
        "package src\n\nfunc A() {}\n\nfunc use() { A() }\n",
    )
    .expect("a.go");
    fs::write(root.join("arthron.toml"), "include = [\"src\"]\n").expect("arthron.toml");
}

/// Run the binary with its stderr pointed at a device every write fails on.
fn with_full_stderr(args: &[&str]) -> Output {
    let full = File::create("/dev/full").expect("/dev/full opens for writing");
    Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::from(full))
        .output()
        .expect("running the arthron binary")
}

#[test]
fn an_advisory_that_cannot_be_written_does_not_take_the_answer_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);

    let out = with_full_stderr(&[
        "scan",
        root.to_str().expect("a utf-8 temp path"),
        "--db",
        root.join("graph.redb").to_str().expect("a utf-8 temp path"),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a full stderr turned a finished scan into a crash",
    );
    assert!(
        !out.stdout.is_empty(),
        "the answer was produced and then thrown away",
    );

    // The same run with a stderr that works: the advisory is real, so the case
    // above is the write failing and not the line never being written.
    let out = Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args([
            "scan",
            root.to_str().expect("a utf-8 temp path"),
            "--db",
            root.join("again.redb").to_str().expect("a utf-8 temp path"),
        ])
        .output()
        .expect("running the arthron binary");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("`include` is set"), "{stderr}");
}

#[test]
fn an_error_nobody_can_read_still_leaves_the_exit_code_telling_the_truth() {
    // The other half of the rule: swallowing the write must not swallow the
    // failure. Nothing was measured here, so the code has to say so even
    // though the sentence explaining it went nowhere.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    fs::write(root.join("arthron.toml"), "exlude = [\"src\"]\n").expect("arthron.toml");

    let out = with_full_stderr(&["scan", root.to_str().expect("a utf-8 temp path")]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a usage error with an unwritable stderr reported something else",
    );
}

#[test]
fn a_gate_failure_survives_a_stderr_it_cannot_write_to() {
    // The gate writes its verdict lines to stderr after the report has gone to
    // stdout, and a regression's whole value is the exit code CI reads.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("go.mod"), "module example.com/app\n\ngo 1.22\n").expect("go.mod");
    fs::write(
        root.join("a.go"),
        "package app\n\nfunc A() {}\n\nfunc use() { A() }\n",
    )
    .expect("a.go");
    let baseline = root.join("go.toml");
    fs::write(
        &baseline,
        concat!(
            "format = 1\n",
            "corpus = \"fixture\"\n",
            "commit = \"unknown\"\n",
            "language = \"go\"\n",
            "resolved = 99\n",
            "external = 0\n",
            "local_binding = 0\n",
            "unresolved = 0\n",
        ),
    )
    .expect("baseline");

    let out = with_full_stderr(&[
        "gate",
        root.to_str().expect("a utf-8 temp path"),
        "--language",
        "go",
        "--baseline",
        baseline.to_str().expect("a utf-8 temp path"),
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a gate regression with an unwritable stderr reported something else",
    );
}
