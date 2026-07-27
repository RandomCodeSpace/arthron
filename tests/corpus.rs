//! Milestone acceptance: a non-zero, honest resolution rate on the corpus.

use arthron::model::{Lang, reason_name};
use arthron::pipeline::scan;

#[test]
fn corpus_rate_is_nonzero_and_every_unresolved_has_a_reason() {
    let corpus = std::path::Path::new("corpus/go/codeiq");
    if !corpus.join("go.mod").is_file() {
        // The corpus lives in RandomCodeSpace/arthron-corpus, cloned into
        // ./corpus (gitignored). Skipping is correct when it is absent —
        // failing would make an unfetched corpus look like a broken engine.
        println!("SKIP: no corpus at {} — see README", corpus.display());
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
