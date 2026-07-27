//! Milestone acceptance: a non-zero, honest resolution rate on the corpus.

use std::fs;
use std::path::{Path, PathBuf};

use arthron::extract_go::extract;
use arthron::model::{Lang, reason_name};
use arthron::pipeline::{scan_go, source_files};
use arthron::resolve_go::GoLang;
use arthron::store::Store;

/// Whether the corpus has been cloned in.
///
/// It lives in RandomCodeSpace/arthron-corpus, cloned into ./corpus
/// (gitignored). Skipping is correct when it is absent — failing would make
/// an unfetched corpus look like a broken engine.
fn corpus_present(corpus: &Path) -> bool {
    if corpus.join("go.mod").is_file() {
        return true;
    }
    println!("SKIP: no corpus at {} — see README", corpus.display());
    false
}

/// Count the references in the corpus by extracting it again, independently
/// of the pipeline.
///
/// This deliberately does not ask the pipeline how many references it found:
/// a bug that loses one between the extractor and the store would lose it
/// from both sides of the comparison and the assertion would pass. It shares
/// only the two things it must in order to be comparing the same corpus at
/// all — [`extract`], and [`source_files`] for the file set.
fn extracted_reference_count(corpus: &Path) -> u64 {
    let mut total = 0u64;
    for path in source_files::<GoLang>(corpus).expect("walking the corpus") {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let facts = extract(&rel, &source);
        total += facts.refs.len() as u64;
    }
    total
}

#[test]
fn corpus_rate_is_nonzero_and_every_unresolved_has_a_reason() {
    let corpus = Path::new("corpus/go/codeiq");
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let report = scan_go(corpus, &dir.path().join("graph.redb")).expect("scan");
    let go = &report.per_lang[&Lang::Go.code()];

    let unresolved = go.unresolved_total();
    let rate = arthron::resolution_rate(go.resolved, unresolved)
        .expect("the corpus has references to measure");

    println!(
        "resolved {} external {} local-binding {} unresolved {}",
        go.resolved, go.external, go.local_binding, unresolved
    );
    for (code, count) in &go.unresolved {
        println!("  {}: {count}", reason_name(*code));
    }
    println!("rate {:.1}%", rate * 100.0);

    // The definition of done: non-zero and honest. The predecessor's
    // baseline on this exact code was 0.0%.
    assert!(rate > 0.0, "resolution rate must beat the 0% baseline");
    assert!(go.resolved > 0);
    assert!(
        unresolved > 0,
        "a skeleton claiming 100% is lying somewhere"
    );
}

#[test]
fn every_corpus_reference_has_exactly_one_stored_outcome() {
    // "The resolver never drops" is the project's central claim, and a rate
    // is no evidence for it: silently discarding the references it cannot
    // link would *raise* the rate. The reported columns partition the
    // extracted references, so their sum is the reference count — exactly.
    // Under-counting is a dropped reference; over-counting is one reference
    // reported as two outcomes. Both break the contract.
    //
    // `local_binding` is one of the columns even though it is outside both
    // terms of the rate: it is excluded from the *measurement*, never from
    // the *accounting*. Leaving it out here is precisely how moving
    // references into it could look like an improvement.
    let corpus = Path::new("corpus/go/codeiq");
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let report = scan_go(corpus, &dir.path().join("graph.redb")).expect("scan");
    let go = &report.per_lang[&Lang::Go.code()];

    let stored = go.resolved + go.external + go.local_binding + go.unresolved_total();
    let extracted = extracted_reference_count(corpus);
    println!("stored outcomes {stored}, extracted references {extracted}");
    assert_eq!(
        stored,
        extracted,
        "resolved {} + external {} + local-binding {} + unresolved {} must \
         equal the {extracted} references the extractor found — every \
         reference gets exactly one stored outcome",
        go.resolved,
        go.external,
        go.local_binding,
        go.unresolved_total(),
    );
}

/// Copy a tree so an event has something to edit.
///
/// `corpus/` is pinned test data and is never written to; every file event
/// below happens to a copy.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap_or_else(|e| panic!("creating {}: {e}", to.display()));
    for entry in fs::read_dir(from).unwrap_or_else(|e| panic!("reading {}: {e}", from.display())) {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target)
                .unwrap_or_else(|e| panic!("copying {}: {e}", entry.path().display()));
        }
    }
}

/// The largest Go file in the tree: a deterministic pick, and the one whose
/// definitions the rest of the corpus is likeliest to reference.
fn largest_file(root: &Path) -> PathBuf {
    source_files::<GoLang>(root)
        .expect("walking the tree")
        .into_iter()
        .max_by_key(|path| {
            let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            // Path breaks the tie, so the pick does not depend on walk order.
            (size, path.clone())
        })
        .expect("the corpus has Go files")
}

/// Scan `tree` cold into a throwaway store and compare it, whole, with what
/// the incremental scans left in `warm_db`.
///
/// Compared **after every event**, not once at the end: a delete followed by
/// a restore puts every identity back, so a store that went stale in between
/// looks correct again by the time the sequence finishes. The intermediate
/// state is the one that has anything to say.
fn assert_matches_cold(tree: &Path, warm_db: &Path, event: &str) {
    let cold_dir = tempfile::tempdir().unwrap();
    let cold_db = cold_dir.path().join("cold.redb");
    let cold_report = scan_go(tree, &cold_db).expect("cold scan");
    let cold = Store::open(&cold_db)
        .expect("open cold")
        .snapshot()
        .unwrap();

    let warm_store = Store::open(warm_db).expect("open warm");
    let warm = warm_store.snapshot().unwrap();
    let warm_report = warm_store.report().unwrap();

    println!(
        "after {event}: {} files, {} nodes, {} rows, {} edges",
        warm.files.len(),
        warm.nodes.len(),
        warm.rows.len(),
        warm.edges.len(),
    );
    assert_eq!(
        cold.files.len(),
        warm.files.len(),
        "after {event}, known files: cold {} warm {}",
        cold.files.len(),
        warm.files.len(),
    );
    for (key, value) in &cold.rows {
        match warm.rows.get(key) {
            None => panic!("after {event}, a cold scan holds row {key:?} => {value:?}"),
            Some(w) => assert!(
                w == value,
                "after {event}, row {key:?}\n  cold {value:?}\n  warm {w:?}"
            ),
        }
    }
    let extra: Vec<_> = warm
        .rows
        .keys()
        .filter(|k| !cold.rows.contains_key(k))
        .collect();
    assert!(
        extra.is_empty(),
        "after {event}, rows a cold scan does not hold: {extra:?}",
    );
    for (id, record) in &cold.nodes {
        assert_eq!(
            warm.nodes.get(id),
            Some(record),
            "after {event}, node {id:?}"
        );
    }
    assert_eq!(cold.nodes.len(), warm.nodes.len(), "after {event}");
    assert_eq!(cold.edges, warm.edges, "after {event}");
    assert_eq!(cold.candidates, warm.candidates, "after {event}");
    assert_eq!(cold, warm, "after {event}, the snapshots differ");
    assert_eq!(cold_report, warm_report, "after {event}");
}

#[test]
fn an_incremental_event_on_the_corpus_lands_a_cold_scans_store() {
    // The oracle at real scale. `tests/incremental.rs` proves the same
    // property on four hand-written files, where every candidate is visible
    // by eye; this proves it where a stale row, a dangling candidate entry
    // or a node one file too many declares would hide.
    let corpus = Path::new("corpus/go/codeiq");
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    copy_tree(corpus, &tree);

    let warm_db = dir.path().join("warm.redb");
    scan_go(&tree, &warm_db).expect("first scan");

    let victim = largest_file(&tree);
    let rel = victim.strip_prefix(&tree).unwrap().display().to_string();
    let original = fs::read_to_string(&victim).expect("reading the file to edit");

    // A hash change that changes no fact: nothing may be woken, and nothing
    // may be lost either.
    fs::write(
        &victim,
        format!("{original}\n// arthron incremental oracle\n"),
    )
    .expect("touching the file");
    scan_go(&tree, &warm_db).expect("touch");
    assert_matches_cold(&tree, &warm_db, &format!("a comment appended to {rel}"));

    // Every definition in the busiest file of the corpus disappears at once.
    // Whatever referenced them sits in files this event never reads as
    // changed — the candidate index is the only thing that can name them.
    fs::remove_file(&victim).expect("deleting the file");
    scan_go(&tree, &warm_db).expect("delete");
    assert_matches_cold(&tree, &warm_db, &format!("{rel} deleted"));

    // And comes back, byte for byte: every identity the delete destroyed is
    // created again, and the references that went unresolved have to find it.
    fs::write(&victim, &original).expect("restoring the file");
    scan_go(&tree, &warm_db).expect("restore");
    assert_matches_cold(&tree, &warm_db, &format!("{rel} restored"));
}

#[test]
fn deleting_a_file_from_the_collision_corpus_lands_a_cold_scans_store() {
    // The same oracle against the corpus whose definitions collide: caddy
    // holds 28 FQNs that two files each declare. A node one file too many
    // declares, or one too few, is invisible in every tally — they are
    // summed from per-file rows — and the per-file replace path is exactly
    // where such a node is either kept or lost.
    let corpus = Path::new("corpus/go/caddy");
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    copy_tree(corpus, &tree);

    let warm_db = dir.path().join("warm.redb");
    scan_go(&tree, &warm_db).expect("first scan");

    let victim = largest_file(&tree);
    let rel = victim.strip_prefix(&tree).unwrap().display().to_string();
    fs::remove_file(&victim).expect("deleting the file");
    scan_go(&tree, &warm_db).expect("delete");
    assert_matches_cold(&tree, &warm_db, &format!("{rel} deleted"));
}
