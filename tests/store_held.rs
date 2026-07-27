//! What a store already held for writing does to a second opener.
//!
//! redb takes an exclusive `flock(2)` on the database file, and every
//! assertion here is bounded in wall-clock time. That bound *is* the test: a
//! refusal and a hang are both "the call has not returned `Ok`", and only a
//! clock tells them apart. A blocking lock — redb switching from `try_lock`
//! to `lock`, or this code growing a retry loop — would turn a second scan
//! into a process that waits forever on the first, and nothing but a timeout
//! catches that.
//!
//! `flock` conflicts are per open file description rather than per process,
//! so a second open *inside* one process is refused by exactly the mechanism
//! that refuses a second process. Both are covered: the in-process cases pin
//! the message, and the CLI case pins that a person running two scans at once
//! sees it.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use arthron::store::{HELD_FOR_WRITING, ReadStore, Store};

/// How long an open may take before this file calls it a hang.
///
/// Generous on purpose: the refusal is a failed `flock`, which takes
/// microseconds, so anything near this bound is a lock being waited on rather
/// than a slow machine.
const BOUND: Duration = Duration::from_secs(10);

/// Run `open` on its own thread and fail the test if it has not answered
/// within [`BOUND`].
///
/// The thread is abandoned on a timeout rather than joined — a thread parked
/// in `flock` cannot be woken, and the panic is the result.
fn within_bound<T: Send + 'static>(
    what: &str,
    open: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(open());
    });
    match rx.recv_timeout(BOUND) {
        Ok(answer) => answer,
        Err(_) => panic!(
            "{what} had not answered after {BOUND:?}: it is waiting on the lock, not refusing it",
        ),
    }
}

/// A minimal Go repository, so `arthron scan` has something to walk.
fn fixture(root: &Path) {
    fs::write(root.join("go.mod"), "module example.com/app\n\ngo 1.22\n").expect("go.mod");
    fs::write(root.join("main.go"), "package main\n\nfunc main() {}\n").expect("main.go");
}

#[test]
fn a_second_write_open_of_a_held_store_is_refused_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("graph.redb");
    let held = Store::open(&db).expect("the store is created");

    let again = db.clone();
    // `let Err(..) else` rather than `expect_err`: a `Store` is not `Debug`,
    // deliberately — there is nothing about an open handle worth printing.
    let Err(err) = within_bound("a second Store::open", move || Store::open(&again)) else {
        panic!("a held store must not open a second writer");
    };

    // Named, not passed through: redb's own words are `Database already open.
    // Cannot acquire lock.`, which name neither the file nor the holder.
    assert!(
        err.contains(HELD_FOR_WRITING),
        "the error has to say why: {err}"
    );
    assert!(
        err.contains(&db.display().to_string()),
        "the error has to say which store: {err}",
    );

    // And the refusal is a lock, not a corruption: it ends with the holder.
    drop(held);
    Store::open(&db).expect("the store opens once the writer is gone");
}

#[test]
fn a_read_only_open_of_a_write_held_store_is_refused_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("graph.redb");
    let held = Store::open(&db).expect("the store is created");

    let again = db.clone();
    let Err(err) = within_bound("ReadStore::open", move || ReadStore::open(&again)) else {
        panic!("a write-held store must not open for reading");
    };

    assert!(
        err.contains(HELD_FOR_WRITING),
        "the error has to say why: {err}"
    );
    assert!(
        err.contains(&db.display().to_string()),
        "the error has to say which store: {err}",
    );

    drop(held);
    ReadStore::open(&db).expect("the store opens once the writer is gone");
}

#[test]
fn a_second_process_scanning_a_held_store_is_refused_rather_than_queued() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");
    let held = Store::open(&db).expect("the store is created");

    let mut child = Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args([
            "scan",
            root.to_str().expect("a utf-8 temp path"),
            "--db",
            db.to_str().expect("a utf-8 temp path"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running the arthron binary");

    let deadline = Instant::now() + BOUND;
    loop {
        match child.try_wait().expect("polling the second scan") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("`arthron scan` sat on the held store for {BOUND:?} instead of refusing");
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }

    let out = child.wait_with_output().expect("the second scan's output");
    assert!(
        !out.status.success(),
        "a scan that could not open the store must not report success",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(HELD_FOR_WRITING),
        "the second scan has to say a scan already holds the store: {stderr}",
    );
    drop(held);
}
