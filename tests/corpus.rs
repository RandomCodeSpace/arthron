//! Milestone acceptance: a non-zero, honest resolution rate on the corpus.

use std::path::Path;

use arthron::extract_go::extract;
use arthron::model::{Lang, reason_name};
use arthron::pipeline::{go_files, scan};

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
/// all — [`extract`], and [`go_files`] for the file set.
fn extracted_reference_count(corpus: &Path) -> u64 {
    let mut total = 0u64;
    for path in go_files(corpus).expect("walking the corpus") {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let facts = extract(&source);
        total += facts.imports.len() as u64 + facts.calls.len() as u64;
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
    let report = scan(corpus, &dir.path().join("graph.redb")).expect("scan");
    let go = &report.per_lang[&Lang::Go.code()];

    let unresolved = go.unresolved_total();
    let rate = arthron::resolution_rate(go.resolved, unresolved)
        .expect("the corpus has references to measure");

    println!(
        "resolved {} external {} unresolved {}",
        go.resolved, go.external, unresolved
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
    // link would *raise* the rate. The three outcome columns partition the
    // extracted references, so their sum is the reference count — exactly.
    // Under-counting is a dropped reference; over-counting is one reference
    // reported as two outcomes. Both break the contract.
    let corpus = Path::new("corpus/go/codeiq");
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let report = scan(corpus, &dir.path().join("graph.redb")).expect("scan");
    let go = &report.per_lang[&Lang::Go.code()];

    let stored = go.resolved + go.external + go.unresolved_total();
    let extracted = extracted_reference_count(corpus);
    println!("stored outcomes {stored}, extracted references {extracted}");
    assert_eq!(
        stored,
        extracted,
        "resolved {} + external {} + unresolved {} must equal the {extracted} \
         references the extractor found — every reference gets exactly one \
         stored outcome",
        go.resolved,
        go.external,
        go.unresolved_total(),
    );
}
