//! A file the filesystem will not hand over is data, not a dead scan.
//!
//! Three ways a tree refuses to be read — no permission on a file, bytes that
//! are not UTF-8, no permission on a directory — and one rule for all three:
//! the walk keeps going, every other file is still measured, and the files
//! that were not are named in the report. Silence is the failure mode this
//! guards: a rate computed over a smaller file set than the tree holds looks
//! exactly like a rate computed over all of it.
//!
//! Two limits on that rule, guarded here too. A file stepped over keeps its
//! facts but loses the store's claim that they are current, so the next scan
//! that can read it re-reads it — otherwise a file whose bytes never move
//! keeps rows resolved against a graph that has since changed, and no report
//! ever says so again. And the scanned root is not a file the walk can step
//! over: a root that is not there measured nothing, which is a failure and
//! not a tree of zeros.
//!
//! The permission cases need a process that permissions apply to. Running as
//! root reads a `chmod 000` file happily, so each of those tests checks that
//! the mode actually took and says so instead of asserting something the
//! kernel was never going to do.
//!
//! # Why this whole file is Unix-only
//!
//! Every refusal here is expressed in Unix terms: `chmod 000` for the two
//! permission cases, and a filename that is bytes rather than UTF-16 for the
//! unkeyable-path case. `std::os::unix` is `#[cfg(unix)]` in std, so the
//! imports below do not merely fail their assertions off Unix — they fail to
//! *compile*, which takes the whole test binary with them and stops
//! `cargo test` before it runs anything. The gate is on the file rather than
//! on the imports so that a Windows runner builds an empty binary and moves
//! on to the rest of the suite. What is guarded here is not skipped on
//! Windows so much as inexpressible on it; a Windows equivalent would be a
//! different test with different mechanics, not a `cfg` arm of this one.

#![cfg(unix)]

// Only the Linux-gated non-UTF-8-filename test spells a name in raw bytes,
// so its two imports carry the same gate.
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use arthron::config::Config;
use arthron::model::Lang;
use arthron::pipeline::scan_repo;
use arthron::store::{Report, Store};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    fs::write(path, content).expect("write");
}

/// `app#Handle → util#Parse`, across two packages: enough that the scan has a
/// real resolved reference to still report when a third file will not open.
fn fixture(root: &Path) {
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "util/util.go",
        "package util\n\nfunc Parse(s string) string { return s }\n",
    );
    write(
        root,
        "app/app.go",
        concat!(
            "package app\n\n",
            "import \"example.com/app/util\"\n\n",
            "func Handle() {\n",
            "\tutil.Parse(\"x\")\n",
            "}\n",
        ),
    );
}

/// Take every permission off `path`, and say whether that actually made it
/// unreadable to this process.
fn make_unreadable(path: &Path) -> bool {
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    fs::read_dir(path)
        .map(|_| ())
        .or_else(|_| fs::read(path).map(|_| ()))
        .is_err()
}

/// Put the permissions back, so the temporary directory can be removed.
fn restore(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod back");
}

fn errored(report: &Report, path: &str) -> Option<String> {
    report
        .file_errors
        .iter()
        .find(|e| e.path == path)
        .map(|e| e.message.clone())
}

fn resolved(report: &Report) -> u64 {
    report
        .per_lang
        .get(&Lang::Go.code())
        .map_or(0, |tally| tally.resolved)
}

fn db(root: &Path) -> PathBuf {
    root.join("graph.redb")
}

#[test]
fn a_file_that_is_not_utf8_is_named_and_the_rest_of_the_tree_is_still_measured() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    // Valid Go, with one byte no UTF-8 decoder will take. A lone 0xFF cannot
    // begin a sequence, so this is a decode failure and not a parse failure —
    // the extractor never sees it.
    fs::write(
        root.join("util/latin1.go"),
        b"package util\n\n// caf\xe9\nfunc Latin() {}\n",
    )
    .expect("writing the undecodable file");

    let report = scan_repo(root, &db(root)).expect("an undecodable file must not fail the scan");

    let message = errored(&report, "util/latin1.go")
        .unwrap_or_else(|| panic!("the undecodable file is not in {:?}", report.file_errors));
    assert!(
        message.contains("UTF-8"),
        "the message has to say what failed: {message}",
    );
    assert_eq!(report.file_errors.len(), 1, "{:?}", report.file_errors);
    // The point of continuing: the reference in the files that *did* read is
    // still resolved and still counted.
    assert!(resolved(&report) > 0, "{report:?}");
}

#[test]
fn a_file_with_no_read_permission_is_named_and_the_rest_of_the_tree_is_still_measured() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    let locked = root.join("util/locked.go");
    fs::write(&locked, "package util\n\nfunc Locked() {}\n").expect("writing the locked file");
    if !make_unreadable(&locked) {
        eprintln!("skipped: this process reads a chmod 000 file (running as root?)");
        return;
    }

    let report = scan_repo(root, &db(root)).expect("an unreadable file must not fail the scan");
    restore(&locked, 0o644);

    let message = errored(&report, "util/locked.go")
        .unwrap_or_else(|| panic!("the unreadable file is not in {:?}", report.file_errors));
    assert!(
        message.contains("util/locked.go"),
        "the message has to name the file: {message}",
    );
    assert_eq!(report.file_errors.len(), 1, "{:?}", report.file_errors);
    assert!(resolved(&report) > 0, "{report:?}");
}

#[test]
fn a_directory_the_walk_cannot_descend_into_is_named_once_and_the_scan_continues() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    let closed = root.join("closed");
    fs::create_dir(&closed).expect("mkdir closed");
    fs::write(closed.join("hidden.go"), "package closed\n").expect("writing inside it");
    if !make_unreadable(&closed) {
        eprintln!("skipped: this process reads a chmod 000 directory (running as root?)");
        return;
    }

    let report =
        scan_repo(root, &db(root)).expect("an unreadable directory must not fail the scan");
    restore(&closed, 0o755);

    // Every live track walks the tree, so every one of them hits this
    // directory. One file, one entry: the report counts files it could not
    // read, not attempts to read them.
    assert_eq!(
        report
            .file_errors
            .iter()
            .filter(|e| e.path == "closed")
            .count(),
        1,
        "{:?}",
        report.file_errors,
    );
    assert!(resolved(&report) > 0, "{report:?}");
}

#[test]
fn an_unreadable_file_keeps_the_facts_it_produced_when_it_could_be_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    let locked = root.join("util/later.go");
    fs::write(&locked, "package util\n\nfunc Later() {}\n").expect("writing the file");

    let first = scan_repo(root, &db(root)).expect("the first scan reads everything");
    assert!(first.file_errors.is_empty(), "{:?}", first.file_errors);
    let before = Store::open(&db(root))
        .expect("the store opens")
        .known_files()
        .expect("the file list");
    assert!(before.contains(&"util/later.go".to_string()), "{before:?}");

    if !make_unreadable(&locked) {
        eprintln!("skipped: this process reads a chmod 000 file (running as root?)");
        return;
    }
    let second = scan_repo(root, &db(root)).expect("the second scan tolerates it");
    restore(&locked, 0o644);

    assert!(
        errored(&second, "util/later.go").is_some(),
        "{:?}",
        second.file_errors,
    );
    // A file that will not open is not a file that is gone. Forgetting its
    // facts here would delete a definition — and every edge into it — because
    // of a permission bit.
    let after = Store::open(&db(root))
        .expect("the store opens")
        .known_files()
        .expect("the file list");
    assert!(
        after.contains(&"util/later.go".to_string()),
        "an unreadable file must not be treated as deleted: {after:?}",
    );
}

#[test]
fn an_unreadable_file_is_re_read_by_the_next_scan_that_can_read_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);

    let first = scan_repo(root, &db(root)).expect("the first scan reads everything");
    assert!(first.file_errors.is_empty(), "{:?}", first.file_errors);

    // The file goes unreadable, and *while it is*, the definition it calls is
    // renamed. Its stored row still says `Resolved`, and its own bytes never
    // move — so nothing about the file itself will ever look changed again.
    let locked = root.join("app/app.go");
    if !make_unreadable(&locked) {
        eprintln!("skipped: this process reads a chmod 000 file (running as root?)");
        return;
    }
    write(
        root,
        "util/util.go",
        "package util\n\nfunc Parsed(s string) string { return s }\n",
    );
    let second = scan_repo(root, &db(root)).expect("the second scan tolerates it");
    assert!(
        errored(&second, "app/app.go").is_some(),
        "{:?}",
        second.file_errors,
    );

    restore(&locked, 0o644);
    let third = scan_repo(root, &db(root)).expect("the third scan reads it again");
    assert!(third.file_errors.is_empty(), "{:?}", third.file_errors);

    // The whole store, not the tally: a stale row that happens to sum the
    // same way is the failure this is guarding, so the comparison is against
    // everything a cold scan of the very same tree builds — rows, edges and
    // the nodes those edges reach.
    let cold_dir = tempfile::tempdir().expect("tempdir");
    let cold_db = cold_dir.path().join("cold.redb");
    let cold_report = scan_repo(root, &cold_db).expect("a cold scan of the same tree");
    let cold = Store::open(&cold_db)
        .expect("the cold store opens")
        .snapshot()
        .expect("the cold snapshot");
    let warm = Store::open(&db(root))
        .expect("the warm store opens")
        .snapshot()
        .expect("the warm snapshot");

    assert_eq!(
        cold_report, third,
        "a scan that stepped over a file must not report it as measured once it can read it",
    );
    assert_eq!(
        cold, warm,
        "the store a recovered scan leaves must be the store a cold scan builds",
    );
}

#[test]
fn a_root_the_walk_cannot_reach_fails_the_scan_instead_of_measuring_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("no-such-tree");

    // Not a file the walk stepped over: nothing was measured at all, and a
    // report of zeros is what a gate would read as a clean corpus.
    let err = scan_repo(&missing, &dir.path().join("graph.redb"))
        .expect_err("a root that does not exist is not a measurement");
    assert!(
        err.contains(&missing.display().to_string()),
        "the error has to say which root: {err}",
    );
}

// Linux only, not merely Unix: APFS enforces valid-UTF-8 filenames, so the
// fixture write itself fails with "Illegal byte sequence" (os error 92) on
// macOS — the input this test needs cannot exist there. That is the platform
// refusing the input, so there is nothing to test on macOS; this is not a
// skip of a real scenario.
#[test]
#[cfg(target_os = "linux")]
fn a_path_that_is_not_utf8_is_named_rather_than_keyed_lossily() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    // `to_string_lossy` maps every undecodable byte onto U+FFFD, so these two
    // files have one repo-relative path between them. Reading either would
    // key its facts under a name the other also answers to, and the second
    // scanned would replace the first's definitions without saying so.
    for byte in [
        b"util/lossy\xfe.go".as_slice(),
        b"util/lossy\xff.go".as_slice(),
    ] {
        fs::write(
            root.join(OsStr::from_bytes(byte)),
            "package util\n\nfunc Lossy() {}\n",
        )
        .expect("writing a file whose name is not utf-8");
    }

    let report = scan_repo(root, &db(root)).expect("an undecodable path must not fail the scan");

    let named: Vec<&str> = report
        .file_errors
        .iter()
        .filter(|e| e.message.contains("not valid UTF-8"))
        .map(|e| e.path.as_str())
        .collect();
    // Both are reported, and under spellings that differ: the lossy one they
    // share is the whole reason neither may be read, so the report cannot use
    // it either without folding two files into one entry.
    assert_eq!(named.len(), 2, "{:?}", report.file_errors);
    assert!(named.iter().all(|p| p.contains("lossy")), "{named:?}");
    assert_ne!(named[0], named[1], "{named:?}");
    assert!(resolved(&report) > 0, "{report:?}");

    let known = Store::open(&db(root))
        .expect("the store opens")
        .known_files()
        .expect("the file list");
    assert!(
        !known.iter().any(|f| f.contains("lossy")),
        "an unkeyable file must not reach the store: {known:?}",
    );
}

#[test]
fn the_scan_document_carries_every_file_the_scan_could_not_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    fs::write(
        root.join("util/latin1.go"),
        b"package util\n\n// caf\xe9\nfunc Latin() {}\n",
    )
    .expect("writing the undecodable file");

    let report = scan_repo(root, &db(root)).expect("the scan runs");
    let doc = arthron::json::scan(&report, &Config::default());
    assert_eq!(
        doc["file_errors"][0]["path"], "util/latin1.go",
        "{}",
        doc["file_errors"],
    );
    assert!(
        doc["file_errors"][0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("UTF-8")),
        "{}",
        doc["file_errors"],
    );
}

#[test]
fn the_text_report_says_how_many_files_it_could_not_read_and_names_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    fs::write(
        root.join("util/latin1.go"),
        b"package util\n\n// caf\xe9\nfunc Latin() {}\n",
    )
    .expect("writing the undecodable file");

    let out = Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args([
            "scan",
            root.to_str().expect("a utf-8 temp path"),
            "--db",
            db(root).to_str().expect("a utf-8 temp path"),
        ])
        .output()
        .expect("running the arthron binary");

    // Exit 0: the scan measured the tree it could read, and that is a result.
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("file errors 1"), "{text}");
    assert!(text.contains("util/latin1.go"), "{text}");
}
