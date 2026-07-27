//! Milestone acceptance for the EcmaScript track, and the ratchet that keeps
//! it from sliding.
//!
//! Two corpora, **two baselines, never one**: `arthron gate` compares per
//! language, and one combined EcmaScript number would let a collapse in
//! JavaScript be masked by TypeScript. Each baseline names the language it
//! measures and is compared only against that language's tally.
//!
//! This suite is where the gate runs for this track. `arthron gate` itself
//! cannot be pointed at a JavaScript-only corpus today: `pipeline::scan_repo`
//! propagates the first live track's error, and the Go track's phase 0 fails
//! when there is no `go.mod`. That is a core seam this track may not touch, so
//! the comparison is driven here through the same `gate::evaluate` the command
//! uses — the arithmetic is identical, only the entry point differs.

use std::path::Path;

use arthron::gate::{Baseline, GateVerdict, Measured, evaluate, parse_baseline};
use arthron::model::{Lang, reason_name};
use arthron::store::Report;
use arthron::track_ecma::scan_ecma;

/// Whether a corpus has been cloned in.
///
/// It lives in RandomCodeSpace/arthron-corpus, cloned into ./corpus
/// (gitignored). Skipping is correct when it is absent — failing would make an
/// unfetched corpus look like a broken engine.
fn corpus_present(corpus: &Path) -> bool {
    if corpus.join("package.json").is_file() {
        return true;
    }
    println!(
        "SKIP: no corpus at {} — see corpus/README.md",
        corpus.display()
    );
    false
}

fn measure(corpus: &Path, lang: Lang) -> (Report, Measured) {
    let dir = tempfile::tempdir().unwrap();
    let report = scan_ecma(corpus, &dir.path().join("graph.redb")).expect("scan");
    let tally = report
        .per_lang
        .get(&lang.code())
        .unwrap_or_else(|| panic!("{} has no line in the report", lang.name()))
        .clone();
    let measured = Measured {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "{}: resolved {} external {} local-binding {} unresolved {}",
        lang.name(),
        measured.resolved,
        measured.external,
        measured.local_binding,
        measured.unresolved,
    );
    for (code, count) in &tally.unresolved {
        println!("  {}: {count}", reason_name(*code));
    }
    if let Some(rate) = arthron::resolution_rate(measured.resolved, measured.unresolved) {
        println!("  rate {:.1}%", rate * 100.0);
    }
    (report, measured)
}

fn baseline(path: &str) -> Baseline {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    parse_baseline(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Compare against the committed baseline and fail on any regression — or on
/// any drift in `external` or `local_binding`, which sit outside *both* terms
/// of the rate and are therefore the one way this gate could be raised without
/// anything being linked.
fn gate(corpus: &Path, lang: Lang, baseline_path: &str) {
    if !corpus_present(corpus) {
        return;
    }
    let (_, measured) = measure(corpus, lang);
    let b = baseline(baseline_path);
    assert_eq!(
        b.language,
        lang.name(),
        "{baseline_path} measures another language"
    );
    match evaluate(&b, &measured) {
        GateVerdict::Pass { improved } => {
            if improved {
                println!("improved over {baseline_path} — re-base deliberately");
            }
        }
        GateVerdict::Fail(failures) => {
            panic!("{baseline_path}: {failures:?}\nmeasured {measured:?}")
        }
        GateVerdict::Error(e) => panic!("{baseline_path}: {e}"),
    }
}

#[test]
fn javascript_holds_its_baseline_on_fastify() {
    gate(
        Path::new("corpus/javascript/fastify"),
        Lang::JavaScript,
        "baselines/javascript-fastify.toml",
    );
}

#[test]
fn typescript_holds_its_baseline_on_vue_core() {
    gate(
        Path::new("corpus/typescript/vue-core"),
        Lang::TypeScript,
        "baselines/typescript-vue-core.toml",
    );
}

#[test]
fn a_baseline_is_refused_against_another_languages_scan() {
    // The rule that keeps two rates from becoming one: a baseline names the
    // language it measures, and nothing may compare it against another's.
    let js = baseline("baselines/javascript-fastify.toml");
    let ts = baseline("baselines/typescript-vue-core.toml");
    assert_eq!(js.language, "javascript");
    assert_eq!(ts.language, "typescript");
    assert_ne!(js.language, ts.language);
    // And no third file aggregates them.
    assert!(
        !Path::new("baselines/ecmascript.toml").exists(),
        "a combined EcmaScript baseline would let one language mask the other",
    );
}

#[test]
fn the_unresolved_floor_is_real_on_both_corpora() {
    // The reasons that must stay large. `NeedsTypeInference` and its two
    // siblings are the honest cost of not running a type checker; a scan that
    // reported them near zero would have moved them somewhere they do not
    // belong — `LocalBinding` and `External` are outside *both* rate terms, so
    // routing anything into them raises the rate without linking a thing.
    for (corpus, lang) in [
        (Path::new("corpus/javascript/fastify"), Lang::JavaScript),
        (Path::new("corpus/typescript/vue-core"), Lang::TypeScript),
    ] {
        if !corpus_present(corpus) {
            continue;
        }
        let (report, measured) = measure(corpus, lang);
        let tally = &report.per_lang[&lang.code()];
        let inference: u64 = tally
            .unresolved
            .iter()
            .filter(|(code, _)| {
                matches!(
                    reason_name(**code),
                    "NeedsTypeInference" | "NeedsReceiverType" | "NeedsExpressionType"
                )
            })
            .map(|(_, count)| *count)
            .sum();
        assert!(
            inference > 0,
            "{}: a receiver-type floor of zero would mean it was reclassified",
            lang.name(),
        );
        assert!(measured.resolved > 0, "{}: nothing linked", lang.name());
        assert!(
            measured.unresolved > 0,
            "{}: a scan claiming everything resolved is lying somewhere",
            lang.name(),
        );
    }
}
