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
use arthron::store::{DeclSite, DefBatch, FileDefs, NodeRecord, Snapshot, Store, StoredOutcome};

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

    let mut interrupted = 0;
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
        if torn != truth {
            interrupted += 1;
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
        interrupted > 0,
        "no kill in the sweep landed inside the event, so the sweep asserts \
         nothing: every store was already the finished one",
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
