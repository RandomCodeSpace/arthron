//! What each exit code a command can return actually means.
//!
//! CI reads the number, not the sentence. `scan` used to answer 1 for a store
//! another scan was holding — the same 1 a gate regression answers — so a
//! build could not tell "a colleague is scanning this checkout right now" from
//! "the measurement got worse", and retrying the first while failing the
//! second was impossible. The two are not the same kind of event and must not
//! share a code.
//!
//! The map, for every command:
//!
//! - **0** the command ran and this is the answer.
//! - **1** the command ran and the answer is no: a gate regression, a query
//!   that matched nothing or matched several. Never an error.
//! - **2** there is no verdict. Usually nothing was measured: usage, I/O, or
//!   the environment — a store somebody else holds, a root that is not there,
//!   a directory that cannot be created, a config file that will not parse —
//!   and those are the ones worth retrying. `gate` also answers 2 when the
//!   comparison could not be made at all: a baseline's or a run's `resolved +
//!   unresolved` of zero leaves no rate on that side, which is neither a pass
//!   nor a regression. That one is measured and deterministic, so a retry
//!   answers 2 again.
//!
//! `scan` therefore never returns 1: it has no verdict to fail. Every failure
//! it can have is one of the environmental ones.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use arthron::store::Store;

fn arthron(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args(args)
        .output()
        .expect("running the arthron binary")
}

fn code(out: &Output) -> Option<i32> {
    out.status.code()
}

/// A Go module with one resolved reference, so a scan of it has an answer.
fn fixture(root: &Path) {
    fs::write(root.join("go.mod"), "module example.com/app\n\ngo 1.22\n").expect("go.mod");
    fs::write(
        root.join("a.go"),
        "package app\n\nfunc A() {}\n\nfunc use() { A() }\n",
    )
    .expect("a.go");
}

#[test]
fn a_scan_that_answered_exits_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    let out = arthron(&[
        "scan",
        root.to_str().expect("a utf-8 temp path"),
        "--db",
        root.join("graph.redb").to_str().expect("a utf-8 temp path"),
    ]);
    assert_eq!(
        code(&out),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn a_store_another_scan_is_holding_is_environmental_and_exits_two() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    // The lock is an exclusive `flock(2)` on the database file, and `flock`
    // conflicts per open file description — so a handle held here is refused
    // to the child exactly as another process's would be.
    let held = Store::open(&db).expect("the store is created");

    let out = arthron(&[
        "scan",
        root.to_str().expect("a utf-8 temp path"),
        "--db",
        db.to_str().expect("a utf-8 temp path"),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        code(&out),
        Some(2),
        "a lock collision is not a measured failure: {stderr}",
    );
    assert!(
        stderr.contains("scan is already running"),
        "the code says what kind of problem it is; the message says which: {stderr}",
    );
    drop(held);
}

#[test]
fn a_root_that_is_not_there_exits_two() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("no-such-tree");
    let out = arthron(&[
        "scan",
        missing.to_str().expect("a utf-8 temp path"),
        "--db",
        dir.path()
            .join("graph.redb")
            .to_str()
            .expect("a utf-8 temp path"),
    ]);
    assert_eq!(
        code(&out),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr),
    );
}

/// The same question with no `--db`, which is the invocation the exit-code
/// table is read against. The default store is `<root>/.arthron/graph.redb`,
/// so creating its parent used to create the missing root along with it: the
/// walk then found an empty tree, the run answered 0 with a report of zeros,
/// and the tree that was not there was silently on disk afterwards. Whether a
/// missing root is an error must not depend on where the store sits.
#[test]
fn a_root_that_is_not_there_exits_two_with_the_default_store_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("no-such-tree");
    let out = arthron(&["scan", missing.to_str().expect("a utf-8 temp path")]);
    assert_eq!(
        code(&out),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !missing.exists(),
        "the run created the root whose absence was the failure",
    );
}

#[cfg(unix)]
#[test]
fn a_store_directory_that_cannot_be_created_exits_two() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    let locked = root.join("locked");
    fs::create_dir(&locked).expect("mkdir");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("chmod 500");
    // Running as root writes into a read-only directory happily, so say what
    // was actually tested rather than asserting what the kernel would not do.
    let blocked = fs::create_dir(locked.join("probe")).is_err();
    if blocked {
        let out = arthron(&[
            "scan",
            root.to_str().expect("a utf-8 temp path"),
            "--db",
            locked
                .join("sub/graph.redb")
                .to_str()
                .expect("a utf-8 temp path"),
        ]);
        assert_eq!(
            code(&out),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr),
        );
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("chmod back");
}

#[test]
fn a_config_that_will_not_parse_exits_two() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    fs::write(root.join("arthron.toml"), "include = [\n").expect("arthron.toml");
    let out = arthron(&["scan", root.to_str().expect("a utf-8 temp path")]);
    assert_eq!(code(&out), Some(2));
}

#[test]
fn one_is_the_verdict_and_nothing_else() {
    // The contrast the whole map exists for: the only 1 in a scan-and-gate
    // pipeline is the gate saying the numbers got worse. A scan cannot
    // produce that code at all, so a script may retry a 2 and must never
    // retry a 1.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
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

    let out = arthron(&[
        "gate",
        root.to_str().expect("a utf-8 temp path"),
        "--language",
        "go",
        "--baseline",
        baseline.to_str().expect("a utf-8 temp path"),
    ]);
    assert_eq!(
        code(&out),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr),
    );

    // …and the gate's own environmental failure is a 2, not that 1.
    let out = arthron(&[
        "gate",
        root.to_str().expect("a utf-8 temp path"),
        "--language",
        "go",
        "--baseline",
        root.join("no-such-baseline.toml")
            .to_str()
            .expect("a utf-8 temp path"),
    ]);
    assert_eq!(code(&out), Some(2));
}

#[test]
fn a_comparison_that_could_not_be_made_is_a_two_in_both_output_modes() {
    // The half of exit code 2 that is not environmental, and the one the
    // documentation used to leave out. A baseline whose `resolved +
    // unresolved` is zero has no rate to compare against, so the run is
    // neither a pass (0) nor a regression (1) — and unlike a held store or a
    // missing root it is measured and deterministic, so retrying it cannot
    // help. Both output modes answer through the same mapping and so answer
    // the same number.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    let baseline = root.join("go.toml");
    // `external = 7` so the file is not all zeros: the emptiness under test
    // is the rate's denominator, not the baseline.
    fs::write(
        &baseline,
        concat!(
            "format = 1\n",
            "corpus = \"fixture\"\n",
            "commit = \"unknown\"\n",
            "language = \"go\"\n",
            "resolved = 0\n",
            "external = 7\n",
            "local_binding = 0\n",
            "unresolved = 0\n",
        ),
    )
    .expect("baseline");

    let args = [
        "gate",
        root.to_str().expect("a utf-8 temp path"),
        "--language",
        "go",
        "--baseline",
        baseline.to_str().expect("a utf-8 temp path"),
    ];

    let out = arthron(&args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        code(&out),
        Some(2),
        "a comparison with no rate on one side is not a verdict: {stderr}",
    );
    assert!(
        stderr.contains("gate: error"),
        "the reason is named, not just the code: {stderr}",
    );

    // Deterministic, unlike every other 2: retrying answers 2 again.
    let again = arthron(&args);
    assert_eq!(
        code(&again),
        Some(2),
        "{}",
        String::from_utf8_lossy(&again.stderr),
    );

    // …and `--json` agrees, on the code and in the document.
    let mut json_args = args.to_vec();
    json_args.push("--json");
    let out = arthron(&json_args);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        code(&out),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(stdout.contains("\"verdict\": \"error\""), "{stdout}");
}
