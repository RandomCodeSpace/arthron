//! Milestone acceptance for Java: a real, honest rate on a real corpus, and a
//! ratchet that holds it.
//!
//! The corpus is not vendored here — it lives in `RandomCodeSpace/arthron-corpus`
//! and is cloned into `./corpus` (gitignored). Skipping when it is absent is
//! correct: failing would make an unfetched corpus look like a broken engine.
//!
//! The ratchet is the project's own, reused rather than reimplemented:
//! [`arthron::gate::parse_baseline`] reads the same file format the Go
//! baselines use and [`arthron::gate::evaluate`] performs the same exact
//! integer comparison. It runs here rather than through `arthron gate` because
//! that command measures `Lang::Go` and only `Lang::Go` — see this track's
//! report for the two core gaps that keep Java off the CLI gate.

use std::path::Path;

use arthron::gate::{
    Baseline, Counts, FORMAT, GateVerdict, evaluate, is_renderable, parse_baseline, render_baseline,
};
use arthron::model::{Lang, reason_name};
use arthron::pipeline::source_files;
use arthron::store::Store;
use arthron::track_java::extract::extract;
use arthron::track_java::{JavaLang, scan_java};

const CORPUS: &str = "corpus/java/commons-lang";
const BASELINE: &str = "baselines/java-commons-lang.toml";
/// The pinned corpus revision, for the baseline's provenance line.
const CORPUS_COMMIT: &str = "598dfc1";

/// Whether the corpus has been cloned in.
fn corpus_present(corpus: &Path) -> bool {
    if corpus.join("src/main/java").is_dir() {
        return true;
    }
    println!("SKIP: no corpus at {} — see README", corpus.display());
    false
}

/// Count the corpus's references by extracting it again, independently of the
/// pipeline.
///
/// Deliberately not "ask the pipeline how many it found": a reference lost
/// between the extractor and the store would vanish from both sides of the
/// comparison and the assertion would pass. It shares only what it must to be
/// looking at the same corpus at all — [`extract`] and the file walk.
fn extracted_reference_count(corpus: &Path) -> u64 {
    let mut total = 0u64;
    for path in source_files::<JavaLang>(corpus).expect("walking the corpus") {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        total += extract(&rel, &source).refs.len() as u64;
    }
    total
}

#[test]
fn corpus_rate_is_nonzero_and_every_unresolved_has_a_reason() {
    let corpus = Path::new(CORPUS);
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.redb");
    let report = scan_java(corpus, &db).expect("scan");
    let java = &report.per_lang[&Lang::Java.code()];

    let unresolved = java.unresolved_total();
    let rate = arthron::resolution_rate(java.resolved, unresolved)
        .expect("the corpus has references to measure");

    println!("java rate      {rate:.4}");
    println!("  resolved     {}", java.resolved);
    println!("  unresolved   {unresolved}");
    println!("  external     {}", java.external);
    println!("  localbinding {}", java.local_binding);
    for (code, count) in &java.unresolved {
        println!("  {:<22} {count}", reason_name(*code));
    }
    println!("  fqn_collisions {}", report.fqn_collisions);

    // Every reference the extractor produced has exactly one stored outcome:
    // nothing is dropped between the two halves of the scan.
    let store = Store::open(&db).expect("store opens");
    let rows = store.snapshot().expect("snapshot");
    let stored: u64 = rows.rows.values().map(|r| u64::from(r.count)).sum();
    assert_eq!(
        stored,
        extracted_reference_count(corpus),
        "a reference was lost between extraction and the store",
    );

    // A rate of zero is a measurement, but not an acceptable one for a
    // language whose resolver claims to link anything at all.
    assert!(rate > 0.0, "nothing resolved");
    // Rates are per language and never aggregated: Go must not appear in a
    // report produced by the Java track's own scan of a Java-only tree.
    assert!(!report.per_lang.contains_key(&Lang::Go.code()));
}

/// Measure the corpus once against a cold store.
///
/// The ratchet and the recorder share it so that the file one writes is the
/// number the other compares: two measurement paths would let a baseline be
/// recorded from a scan the gate never performs.
fn measure(corpus: &Path) -> Counts {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let report = scan_java(corpus, &dir.path().join("graph.redb")).expect("scan");
    let java = &report.per_lang[&Lang::Java.code()];
    Counts {
        resolved: java.resolved,
        external: java.external,
        local_binding: java.local_binding,
        unresolved: java.unresolved_total(),
    }
}

#[test]
fn the_ratchet_holds() {
    let corpus = Path::new(CORPUS);
    if !corpus_present(corpus) {
        return;
    }
    let text = std::fs::read_to_string(BASELINE).expect("the baseline is committed");
    let baseline = parse_baseline(&text).expect("the baseline parses");
    assert_eq!(baseline.language, Lang::Java.name());

    let measured = measure(corpus);
    match evaluate(&baseline, &measured) {
        GateVerdict::Pass { .. } => {}
        other => panic!("{other:?}\nmeasured {measured:?}"),
    }
}

/// Record the baseline the ratchet holds.
///
/// Ignored, because it *writes* the ratchet's own reference file; a rebase is
/// a deliberate act, never a side effect of running the suite:
///
/// ```text
/// cargo test --release --test java_corpus -- --ignored --exact the_baseline_is_recorded
/// ```
///
/// It exists because `arthron gate --rebase` cannot record a Java baseline —
/// see this track's report for the two core gaps. It is not a hand-edit: the
/// counts come from [`measure`], the same scan the ratchet runs, and the file
/// is rendered by the project's own [`render_baseline`], the writer the Go
/// baselines already use.
#[test]
#[ignore = "writes the baseline; run deliberately after a measured change"]
fn the_baseline_is_recorded() {
    let corpus = Path::new(CORPUS);
    assert!(corpus_present(corpus), "a rebase needs the corpus");

    let counts = measure(corpus);
    // The same refusal `arthron gate --rebase` makes: a baseline of all zeros
    // looks as authoritative as a correct one and would bless anything.
    assert!(counts.total() > 0, "this scan counted no references at all");

    let baseline = Baseline {
        format: FORMAT,
        corpus: CORPUS.to_string(),
        commit: CORPUS_COMMIT.to_string(),
        language: Lang::Java.name().to_string(),
        counts,
    };
    for value in [&baseline.corpus, &baseline.commit, &baseline.language] {
        assert!(
            is_renderable(value),
            "provenance `{value}` cannot be written"
        );
    }

    let text = render_baseline(&baseline);
    // Read back through the reader before the file is trusted: a rendered
    // baseline that does not parse to what it was rendered from is a file the
    // gate would misread.
    assert_eq!(
        parse_baseline(&text).expect("the rendered baseline parses"),
        baseline,
    );
    std::fs::write(BASELINE, &text).expect("writing the baseline");
    println!("wrote {BASELINE}\n{text}");
}
