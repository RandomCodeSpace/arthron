//! What a scan killed halfway leaves behind.
//!
//! A scan writes a file's facts in halves — definitions first, then, once the
//! whole symbol table stands, references — and each half is committed in its
//! own transaction so that peak memory stays a batch rather than an event.
//! That is not negotiable, and it means the store between two commits is
//! neither the store the scan started with nor the one it would have
//! finished with.
//!
//! What *is* negotiable is whether the store still claims to be current for
//! the work that never happened. Killing a scan after phase 1 has taken a
//! definition away leaves every reference that resolved to it pointing at a
//! node that is gone; if the files holding those references still carry a
//! valid content hash, no later scan has any reason to re-read them, and the
//! graph is wrong for as long as nobody edits those files again. This file
//! is the bound on that: a scan may be interrupted anywhere, and the next
//! scan must produce exactly the store a cold scan of the same tree does.
//!
//! Two tests, because the failure has two halves worth pinning separately.
//! The first drives the tear through the store API, so the interruption
//! point is exact and the test cannot go quiet if the timing drifts. The
//! second kills a real `arthron scan` child at several points across a real
//! event, which is the thing that actually happened.
//!
//! Then the two places a kill leaves something a *later* scan cannot simply
//! finish. The store's own creation is one of them: redb sizes the file and
//! syncs it before writing the magic number that makes the bytes a database,
//! so a process killed inside that window used to leave a file every later
//! open refused — a first cold scan wedging its own store, with `rm` the only
//! way past and nothing saying so. And a store a kill left behind is one the
//! read paths have to speak about rather than either serve or refuse in
//! redb's words.
//!
//! `#![cfg(unix)]` because the second test needs to kill a child process and
//! read the store it left: `Child::kill` is portable, but a Windows process
//! killed mid-write leaves the file mapped until the handle is released, and
//! pinning that behaviour is a different test than this one.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use arthron::pipeline::scan_repo;
use arthron::store::{
    DeclSite, DefBatch, FileDefs, NEEDS_RECOVERY, NOT_A_STORE, NOT_ALL_CURRENT, NodeRecord,
    ReadStore, Snapshot, Store, StoredOutcome,
};

/// Callers in the deterministic test. Enough that a wrong answer is loud in
/// the counts and small enough that the scan is instant.
const FEW: usize = 40;

/// Callers in the child-process test.
///
/// Sized so that one warm event takes long enough for a sleep to land inside
/// it on a loaded machine, and no longer: the sweep runs the event six times.
const MANY: usize = 500;

/// A Go repository whose every caller calls both of `a`'s functions.
///
/// One package declaring two functions and `callers` files calling both is
/// the smallest shape that separates the two failures worth telling apart: a
/// definition going away must take its edges with it, and every file that
/// called it must be re-resolved even though not one of their bytes moved.
fn fixture(root: &Path, callers: usize, with_beta: bool) {
    fs::create_dir_all(root.join("a")).expect("a/");
    fs::create_dir_all(root.join("c")).expect("c/");
    fs::write(root.join("go.mod"), "module fixture\n\ngo 1.22\n").expect("go.mod");
    declare(root, with_beta);
    for i in 0..callers {
        fs::write(
            root.join(format!("c/c{i:04}.go")),
            format!("package c\n\nimport \"fixture/a\"\n\nfunc C{i:04}() {{\n\ta.Alpha()\n\ta.Beta()\n}}\n"),
        )
        .expect("caller");
    }
}

/// Rewrite the declaring file, with or without `Beta`.
fn declare(root: &Path, with_beta: bool) {
    let body = if with_beta {
        "package a\n\nfunc Alpha() {}\n\nfunc Beta() {}\n"
    } else {
        "package a\n\nfunc Alpha() {}\n"
    };
    fs::write(root.join("a/a.go"), body).expect("a/a.go");
}

/// Edges and rows whose target is no longer a node.
///
/// The ground truth for this file. Tallies cannot see it — a row counts as
/// resolved whether or not the identity it names still exists — so a store
/// can report a perfect rate and be a graph with three thousand edges into
/// nothing.
fn dangling(snapshot: &Snapshot) -> usize {
    let edges = snapshot
        .edges
        .iter()
        .filter(|(src, dst, _)| {
            !snapshot.nodes.contains_key(src) || !snapshot.nodes.contains_key(dst)
        })
        .count();
    let rows = snapshot
        .rows
        .values()
        .filter(|row| {
            matches!(&row.outcome, StoredOutcome::Resolved(id) if !snapshot.nodes.contains_key(id))
        })
        .count();
    edges + rows
}

/// The whole store, as one comparable value.
fn snapshot_of(db: &Path) -> Snapshot {
    Store::open(db)
        .expect("the store opens")
        .snapshot()
        .expect("snapshot")
}

/// A cold scan of a tree in the state `with_beta` describes, and the store it
/// leaves: the answer every other store here is measured against.
fn cold(dir: &Path, callers: usize, with_beta: bool) -> Snapshot {
    let root = dir.join("cold");
    let db = dir.join("cold.redb");
    fixture(&root, callers, with_beta);
    scan_repo(&root, &db).expect("cold scan");
    snapshot_of(&db)
}

/// How many occurrences each language resolved and left unresolved, as the
/// report counts them — the numbers a person reads off a scan.
fn rates(db: &Path) -> Vec<(u8, u64, u64)> {
    let report = Store::open(db).expect("open").report().expect("report");
    report
        .per_lang
        .iter()
        .map(|(lang, tally)| (*lang, tally.resolved, tally.unresolved_total()))
        .collect()
}

/// One file's phase-1 half, read back out of a store that holds it.
///
/// This is what `phase_one` would produce for the file, without asking this
/// test to know a single thing about Go's naming: every node the store says
/// the file declares, carrying that file's declaration sites and no others,
/// minus the identity `dropped` names. External nodes are left out because
/// phase 1 never emits one.
fn definition_half(snapshot: &Snapshot, file: &str, dropped: &str) -> DefBatch {
    let mut nodes = Vec::new();
    for (id, record) in &snapshot.nodes {
        let sites: Vec<DeclSite> = record
            .declarations()
            .iter()
            .filter(|site| site.file == file)
            .cloned()
            .collect();
        if sites.is_empty() {
            continue;
        }
        let rebuilt = match record {
            NodeRecord::Definition {
                fqn,
                kind,
                facets,
                targets,
                ..
            } => {
                if fqn.ends_with(dropped) {
                    continue; // the declaration this edit takes away
                }
                NodeRecord::Definition {
                    fqn: fqn.clone(),
                    kind: *kind,
                    facets: *facets,
                    targets: targets.clone(),
                    declarations: sites,
                }
            }
            NodeRecord::Package {
                import_path, name, ..
            } => NodeRecord::Package {
                import_path: import_path.clone(),
                name: name.clone(),
                declarations: sites,
            },
            NodeRecord::External { .. } => continue,
        };
        nodes.push((*id, rebuilt));
    }
    DefBatch {
        files: vec![FileDefs {
            path: file.to_string(),
            nodes,
        }],
    }
}

#[test]
fn a_definition_phase_the_reference_phase_never_followed_is_healed_by_the_next_scan() {
    let dir = tempfile::tempdir().expect("tempdir");
    let truth = cold(dir.path(), FEW, false);

    let root = dir.path().join("repo");
    let db = dir.path().join("graph.redb");
    fixture(&root, FEW, true);
    scan_repo(&root, &db).expect("first scan");

    // The edit, and phase 1 of it — and then the process dies. `apply_defs`
    // is the exact commit the real scan makes first: `Beta` stops being a
    // node, and not one of the callers has been re-resolved.
    declare(&root, false);
    {
        let store = Store::open(&db).expect("the store opens");
        let half = definition_half(&store.snapshot().expect("snapshot"), "a/a.go", "#Beta");
        store.apply_defs(&half).expect("apply defs");
        let torn = store.snapshot().expect("snapshot");
        assert!(
            dangling(&torn) > 0,
            "the interruption did not tear anything, so this test proves nothing: \
             phase 1 was supposed to take `Beta` away while every caller still \
             resolved to it",
        );
    }

    scan_repo(&root, &db).expect("the next scan");
    let healed = snapshot_of(&db);
    assert_eq!(
        dangling(&healed),
        0,
        "the scan after the interruption left edges pointing at nodes that are gone",
    );
    assert!(
        healed == truth,
        "the scan after the interruption is not the store a cold scan builds: \
         {} nodes / {} edges / {} rows against {} / {} / {}",
        healed.nodes.len(),
        healed.edges.len(),
        healed.rows.len(),
        truth.nodes.len(),
        truth.edges.len(),
        truth.rows.len(),
    );
}

#[test]
fn a_real_scan_killed_across_the_event_is_healed_by_the_next_scan() {
    let dir = tempfile::tempdir().expect("tempdir");
    let truth = cold(dir.path(), MANY, false);
    let truth_rates = rates(&dir.path().join("cold.redb"));

    let root = dir.path().join("repo");
    let pristine = dir.path().join("pristine.redb");
    let db = dir.path().join("graph.redb");
    fixture(&root, MANY, true);
    scan_repo(&root, &pristine).expect("the store the edit starts from");

    // How long the whole event takes on this machine, measured rather than
    // assumed: the sweep below is fractions of it, so a slow CI box moves
    // the kill points with it instead of putting every one past the end.
    declare(&root, false);
    fs::copy(&pristine, &db).expect("copy");
    let started = Instant::now();
    scan_repo(&root, &db).expect("uninterrupted event");
    let event = started.elapsed();
    assert!(
        rates(&db) == truth_rates,
        "the uninterrupted event does not agree with a cold scan, so nothing \
         below can be read as a statement about interruptions",
    );

    // How many kills landed where this test is about: after phase 1 took
    // `Beta` away and before phase 2 put the callers right. `torn != truth`
    // is not that question — the pristine store differs from the truth too
    // (1001 edges against 501), so a kill that landed before the event's
    // first commit satisfies it while proving nothing. A dangling edge or row
    // cannot be left by a kill that wrote nothing and cannot survive one that
    // wrote everything, so it is the sentinel: it is exactly the hazard.
    let mut tore = 0;
    for step in 1..=5 {
        let delay = event.mul_f64(f64::from(step) / 6.0);
        fs::copy(&pristine, &db).expect("copy");
        let mut child = Command::new(env!("CARGO_BIN_EXE_arthron"))
            .arg("scan")
            .arg(&root)
            .arg("--db")
            .arg(&db)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a scan");
        std::thread::sleep(delay);
        // SIGKILL: the strictest interruption there is, and the one arthron
        // cannot influence. It installs no signal handler, so a SIGTERM
        // leaves exactly the same store — there is no cleanup path either
        // signal could have taken.
        let _ = child.kill();
        child.wait().expect("the killed scan is reaped");
        // Give up the store's lock before reading it: the kill releases the
        // `flock` with the process, but the file is only ours once it is
        // reaped, which the wait above guarantees.
        let torn = snapshot_of(&db);
        if dangling(&torn) > 0 {
            tore += 1;
        }

        scan_repo(&root, &db).expect("the next scan");
        let healed = snapshot_of(&db);
        assert_eq!(
            dangling(&healed),
            0,
            "killed {delay:?} into a {event:?} event: the next scan left \
             edges pointing at nodes that are gone",
        );
        assert!(
            healed == truth,
            "killed {delay:?} into a {event:?} event: the next scan is not the \
             store a cold scan builds — {} nodes / {} edges / {} rows against \
             {} / {} / {}",
            healed.nodes.len(),
            healed.edges.len(),
            healed.rows.len(),
            truth.nodes.len(),
            truth.edges.len(),
            truth.rows.len(),
        );
    }
    assert!(
        tore > 0,
        "no kill in the sweep landed between the two phases, so the sweep \
         asserts nothing: every store it read was whole — either the one the \
         event started from or the one it would have finished with",
    );
}

#[test]
fn an_interrupted_addition_is_healed_the_same_way() {
    // The mirror of the deletion, and it fails the other way round: an
    // addition the reference phase never reached leaves every caller
    // unresolved against a definition that is now right there in the store,
    // so the rate is *depressed* rather than inflated and no edge dangles.
    // Silence in the other direction is still silence.
    let dir = tempfile::tempdir().expect("tempdir");
    let truth = cold(dir.path(), FEW, true);

    let root = dir.path().join("repo");
    let db = dir.path().join("graph.redb");
    fixture(&root, FEW, false);
    scan_repo(&root, &db).expect("first scan");

    declare(&root, true);
    {
        let store = Store::open(&db).expect("the store opens");
        let mut half = definition_half(&store.snapshot().expect("snapshot"), "a/a.go", "#nothing");
        // The identity the edit brings in, stated the way phase 1 states one.
        // Taken from the finished store so this test names no FQN grammar.
        let beta = truth
            .nodes
            .iter()
            .find(|(_, record)| {
                matches!(record, NodeRecord::Definition { fqn, .. } if fqn.ends_with("#Beta"))
            })
            .map(|(id, record)| {
                let mut sites = record.declarations().to_vec();
                sites.retain(|site| site.file == "a/a.go");
                (*id, record.clone(), sites)
            })
            .expect("the finished store declares Beta");
        let (id, record, sites) = beta;
        let NodeRecord::Definition {
            fqn, kind, facets, ..
        } = record
        else {
            unreachable!("Beta is a definition")
        };
        half.files[0].nodes.push((
            id,
            NodeRecord::Definition {
                fqn,
                kind,
                facets,
                targets: Vec::new(),
                declarations: sites,
            },
        ));
        store.apply_defs(&half).expect("apply defs");
    }

    scan_repo(&root, &db).expect("the next scan");
    let healed = snapshot_of(&db);
    assert_eq!(dangling(&healed), 0, "an addition left a dangling edge");
    assert!(
        healed == truth,
        "the scan after an interrupted addition is not the store a cold scan \
         builds: {} edges against {}",
        healed.edges.len(),
        truth.edges.len(),
    );
}

/// A repository with a store beside it, the shape most of the tests below
/// want: `dir/repo` holding `callers` callers of both functions, and the
/// path a store would go at.
fn repo_and_db(dir: &Path, callers: usize) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = dir.join("repo");
    fixture(&root, callers, true);
    (root, dir.join("graph.redb"))
}

/// Where a store is built before it is published — the name `Store::open`
/// stages under, restated here so the tests pin it rather than assume it.
fn staging_of(db: &Path) -> std::path::PathBuf {
    let mut name = std::ffi::OsString::from(db.as_os_str());
    name.push(".new");
    std::path::PathBuf::from(name)
}

#[test]
fn a_store_that_does_not_exist_is_published_whole_and_leaves_nothing_beside_it() {
    // The invariant the creation path exists for, stated where a reader will
    // look for it: after a scan there is a store at the path and nothing at
    // the staging name. A staging file that outlived its scan would be the
    // wedge back again, one directory entry over.
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, db) = repo_and_db(dir.path(), FEW);
    scan_repo(&root, &db).expect("the first scan of a repository");

    assert!(db.exists(), "the scan published no store");
    assert!(
        !staging_of(&db).exists(),
        "the staging file outlived the scan that made it",
    );
    ReadStore::open(&db).expect("the published store opens for reading");
}

#[test]
fn a_staging_file_a_killed_creation_left_is_taken_back() {
    // What a process killed inside redb's creation window leaves: bytes at
    // the staging name that are not a database. Nobody holds them and the
    // name is arthron's own, derived from the store the caller asked for, so
    // the next scan takes them back rather than dying on them — which is the
    // whole point of building there instead of at the store's own path.
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, db) = repo_and_db(dir.path(), FEW);
    let staging = staging_of(&db);
    // Sized like the real thing — redb resizes the file before it syncs — but
    // any non-empty file with no magic number is the same refusal.
    fs::write(&staging, vec![0u8; 1 << 20]).expect("a half-made staging file");

    scan_repo(&root, &db).expect("the scan after a killed creation");
    assert!(db.exists(), "the scan published no store");
    assert!(!staging.exists(), "the half-made staging file survived");
    ReadStore::open(&db).expect("the store opens for reading");
}

#[test]
fn bytes_that_are_not_a_store_are_named_rather_than_passed_through_and_left_alone() {
    // The other half of the same failure, and the one arthron must *not*
    // recover from: a store an older build wedged, or a `--db` aimed at a
    // file that was never a graph. Both say so in a sentence with a way out,
    // and neither is deleted — a graph is a cache, but the bytes at a path
    // the caller named are the caller's.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("graph.redb");
    let bytes = b"this is not a graph".to_vec();
    fs::write(&db, &bytes).expect("something that is not a store");

    let Err(write_err) = Store::open(&db) else {
        panic!("bytes that are not a database must not open as a store")
    };
    assert!(write_err.contains(NOT_A_STORE), "{write_err}");
    assert!(write_err.contains(&db.display().to_string()), "{write_err}");

    let read_err = ReadStore::open(&db).expect_err("nor for reading");
    assert!(read_err.contains(NOT_A_STORE), "{read_err}");

    assert_eq!(
        fs::read(&db).expect("the file is still there"),
        bytes,
        "a refused open rewrote the caller's file",
    );
}

#[test]
fn a_path_holding_no_bytes_is_no_store_at_all() {
    // `touch graph.redb` and then scan. An empty file is what redb is willing
    // to make a database out of *in place*, which is the one window this
    // whole path exists to close, so it is treated as the absence it is.
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, db) = repo_and_db(dir.path(), FEW);
    fs::write(&db, b"").expect("touch");

    scan_repo(&root, &db).expect("a scan onto an empty path");
    assert!(!staging_of(&db).exists(), "the staging file survived");
    ReadStore::open(&db).expect("the store opens for reading");
}

#[test]
fn a_query_against_a_store_that_is_not_wholly_current_says_so() {
    // The store knows when it has stopped vouching for a file — that is what
    // an empty `files` row is, and both a scan that could not read a file and
    // a scan killed between a file's two halves leave one. A reader that
    // served those answers without a word would be the silence this whole
    // file is about, one surface over.
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, db) = repo_and_db(dir.path(), FEW);
    scan_repo(&root, &db).expect("first scan");

    let quiet = query_refs(&db, "Alpha");
    assert_eq!(quiet.2, Some(0), "{}", quiet.1);
    assert!(
        !quiet.1.contains(NOT_ALL_CURRENT),
        "a store that is wholly current must say nothing: {}",
        quiet.1,
    );

    {
        let store = Store::open(&db).expect("the store opens");
        store
            .forget_hashes(&["a/a.go".to_string()])
            .expect("withdraw one claim");
    }

    let (stdout, stderr, code) = query_refs(&db, "Alpha");
    assert_eq!(code, Some(0), "the answer is still an answer: {stderr}");
    assert!(
        stdout.contains("references"),
        "stdout is still the answer: {stdout}",
    );
    assert!(
        stderr.contains(NOT_ALL_CURRENT),
        "a store that stopped vouching for a file must say so: {stderr}",
    );
    assert!(
        stderr.contains(&db.display().to_string()),
        "and which store: {stderr}",
    );
}

/// Run `arthron query refs <name>` against a store and hand back what it said.
fn query_refs(db: &Path, name: &str) -> (String, String, Option<i32>) {
    let out = Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args(["query", "refs", name, "--db"])
        .arg(db)
        .output()
        .expect("running the arthron binary");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

/// Whether this machine can put a signal at a chosen syscall.
///
/// `strace --inject` is the only way to kill a scan *exactly* at a commit
/// boundary rather than approximately, by a sleep — and the creation window
/// this file cares about is a millisecond wide, so approximately is no use.
/// It needs `ptrace`, which a container can be built without, so the test
/// that wants it says why it did not run rather than failing a machine that
/// is fine.
fn can_inject() -> bool {
    Command::new("strace")
        .args([
            "-o",
            "/dev/null",
            "-e",
            "inject=fdatasync:signal=SIGKILL:when=4096",
            "/bin/true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Kill `arthron scan` at the `when`th `fdatasync` it reaches, and say
/// whether the kill landed.
fn scan_killed_at_sync(root: &Path, db: &Path, when: u32) -> bool {
    let status = Command::new("strace")
        .args(["-f", "-o", "/dev/null", "-e"])
        .arg(format!("inject=fdatasync:signal=SIGKILL:when={when}"))
        .arg(env!("CARGO_BIN_EXE_arthron"))
        .arg("scan")
        .arg(root)
        .arg("--db")
        .arg(db)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("running arthron under strace");
    !status.success()
}

#[test]
fn a_first_cold_scan_killed_at_any_sync_leaves_a_store_the_next_scan_finishes() {
    // The failure this creation path exists for, end to end and at the exact
    // syscall: the *first* scan of a repository, killed at each sync it
    // reaches. Every one of those used to leave a file no later scan could
    // open — `I/O error: invalid data`, forever, for every kill point across
    // the whole of redb's creation. There is nothing to heal *from* here: a
    // store that was never published is a repository nobody has scanned, and
    // a cold scan is what that asks for.
    if !can_inject() {
        eprintln!("skipped: `strace --inject` does not run here, so a kill cannot be placed");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let truth = cold(dir.path(), FEW, true);
    let root = dir.path().join("repo");
    fixture(&root, FEW, true);

    let mut killed = 0;
    // Past the last sync redb's own creation makes, so the sweep covers the
    // window before publication, the publication, and the schema stamp after
    // it.
    for when in 1..=14 {
        let db = dir.path().join(format!("cold-{when}.redb"));
        if scan_killed_at_sync(&root, &db, when) {
            killed += 1;
        }
        scan_repo(&root, &db).expect("the scan after a killed first scan");
        let healed = snapshot_of(&db);
        assert_eq!(
            dangling(&healed),
            0,
            "killed at sync {when} of a first scan: the next scan left edges              pointing at nodes that are gone",
        );
        assert!(
            healed == truth,
            "killed at sync {when} of a first scan: the next scan is not the              store a cold scan builds — {} nodes / {} edges against {} / {}",
            healed.nodes.len(),
            healed.edges.len(),
            truth.nodes.len(),
            truth.edges.len(),
        );
        assert!(
            !staging_of(&db).exists(),
            "killed at sync {when}: the staging file outlived the scan that healed it",
        );
    }
    assert!(
        killed > 0,
        "no injected kill landed, so this sweep asserts nothing about          interruptions: every scan ran to completion",
    );
}

#[test]
fn a_store_a_kill_left_mid_flight_is_refused_for_reading_by_name() {
    // redb marks a store as needing recovery while a write transaction is in
    // flight, and recovery is a write — so a reader cannot do it and must
    // not. What it *can* do is say which store and what fixes it, rather than
    // hand back `Database repair aborted.` and leave a person guessing at
    // corruption. And the fix has to be true: one scan, after which the
    // reader opens.
    if !can_inject() {
        eprintln!("skipped: `strace --inject` does not run here, so a kill cannot be placed");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let (root, db) = repo_and_db(dir.path(), FEW);
    scan_repo(&root, &db).expect("the store the kill starts from");
    ReadStore::open(&db).expect("a finished scan leaves a store a reader opens");

    declare(&root, false);
    assert!(
        scan_killed_at_sync(&root, &db, 1),
        "the injected kill did not land, so nothing below is about a kill",
    );

    let err = ReadStore::open(&db).expect_err("a store left mid-flight must not be read");
    assert!(err.contains(NEEDS_RECOVERY), "{err}");
    assert!(err.contains(&db.display().to_string()), "{err}");

    scan_repo(&root, &db).expect("the scan the refusal names");
    ReadStore::open(&db).expect("and after it the reader opens");
}
