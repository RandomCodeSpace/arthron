//! Milestone acceptance for the EcmaScript track, and the ratchet that keeps
//! it from sliding.
//!
//! Two corpora, **two baselines, never one**: `arthron gate` compares per
//! language, and one combined EcmaScript number would let a collapse in
//! JavaScript be masked by TypeScript. Each baseline names the language it
//! measures and is compared only against that language's tally.
//!
//! The two baselines here are written by the command, never by hand:
//!
//! ```text
//! arthron gate corpus/javascript/fastify  --language javascript \
//!     --baseline baselines/javascript-fastify.toml  --rebase --commit <pin>
//! arthron gate corpus/typescript/vue-core --language typescript \
//!     --baseline baselines/typescript-vue-core.toml --rebase --commit <pin>
//! ```
//!
//! `--language` is load-bearing and the rendered header comment omits it: the
//! flag defaults to `go`, so re-running the printed command against one of
//! these files would overwrite a JavaScript or TypeScript baseline with the
//! Go tally. The `language = "…"` field records which one is meant, and
//! `a_baseline_is_refused_against_another_languages_scan` below is what makes
//! the mistake fail rather than pass quietly.
//!
//! The comparison also runs here, through the same `gate::evaluate` the
//! command uses, so CI gates this track without building the binary — the
//! arithmetic is identical and only the entry point differs.

use std::path::Path;

use arthron::gate::{Baseline, GateVerdict, Measured, evaluate, parse_baseline};
use arthron::lang::Language;
use arthron::model::{Lang, reason_name};
use arthron::pipeline::source_files;
use arthron::store::Report;
use arthron::track_ecma::extract::extract;
use arthron::track_ecma::lang::{Dialect, JsLang, TsLang};
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

/// Count one language's references in the corpus by extracting it again,
/// independently of the pipeline.
///
/// This deliberately does not ask the pipeline how many references it found:
/// a bug that loses one between the extractor and the store would lose it
/// from both sides of the comparison and the assertion would pass. It shares
/// only the two things it must in order to be counting the same files at all
/// — `extract`, and `source_files` for the file set.
fn extracted_reference_count<L: Language>(corpus: &Path, dialect: Dialect) -> u64 {
    let mut total = 0u64;
    for path in source_files::<L>(corpus).expect("walking the corpus") {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        total += extract(dialect, &rel, &source).refs.len() as u64;
    }
    total
}

/// Both languages' columns must partition both languages' references, on one
/// scan of one corpus.
fn assert_every_reference_is_accounted_for(corpus: &Path) {
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let report = scan_ecma(corpus, &dir.path().join("graph.redb")).expect("scan");
    // Both halves, on every corpus. A corpus with no file of one language
    // still asserts something: `0 == 0` fails the moment a row is tagged with
    // a language whose files are not there, which is exactly how one rate
    // could start borrowing from the other's.
    for (lang, extracted) in [
        (
            Lang::JavaScript,
            extracted_reference_count::<JsLang>(corpus, Dialect::JavaScript),
        ),
        (
            Lang::TypeScript,
            extracted_reference_count::<TsLang>(corpus, Dialect::TypeScript),
        ),
    ] {
        let tally = report
            .per_lang
            .get(&lang.code())
            .cloned()
            .unwrap_or_default();
        let stored =
            tally.resolved + tally.external + tally.local_binding + tally.unresolved_total();
        println!(
            "{} on {}: stored outcomes {stored}, extracted references {extracted}",
            lang.name(),
            corpus.display(),
        );
        assert_eq!(
            stored,
            extracted,
            "{} on {}: resolved {} + external {} + local-binding {} + unresolved {} \
             must equal the {extracted} references the extractor found — every \
             reference gets exactly one stored outcome",
            lang.name(),
            corpus.display(),
            tally.resolved,
            tally.external,
            tally.local_binding,
            tally.unresolved_total(),
        );
    }
}

#[test]
fn every_reference_on_fastify_has_exactly_one_stored_outcome() {
    // "The resolver never drops" is the project's central claim, and a rate is
    // no evidence for it: silently discarding the references it cannot link
    // would *raise* the rate. The four reported columns partition the
    // extracted references, so their sum is the reference count — exactly.
    // Under-counting is a dropped reference; over-counting is one reference
    // reported as two outcomes. Both break the contract.
    //
    // `local_binding` is one of the columns even though it is outside both
    // terms of the rate: it is excluded from the *measurement*, never from the
    // *accounting*. Leaving it out here is precisely how moving references
    // into it could look like an improvement.
    assert_every_reference_is_accounted_for(Path::new("corpus/javascript/fastify"));
}

#[test]
fn every_reference_on_vue_core_has_exactly_one_stored_outcome() {
    assert_every_reference_is_accounted_for(Path::new("corpus/typescript/vue-core"));
}

/// Every triple-slash reference directive in a file, found by reading the
/// source text rather than by asking the extractor.
///
/// An **independent oracle**, and the point of it is what it does not share:
/// `extracted_reference_count` above re-runs the production extractor, so a
/// reference the front end never emits is missing from both sides of that
/// comparison and the assertion passes anyway. That is exactly how
/// `/// <reference … />` went unnoticed — a directive is a comment, no rule
/// selected it, and no bucket ever received it. This function knows only that
/// a directive is a line beginning `///` that names `path=` or `types=`.
fn directives_in(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in source.lines() {
        let line = line.trim_start();
        if !line.starts_with("///") || !line.contains("<reference") {
            continue;
        }
        for attribute in ["path=", "types="] {
            let Some(rest) = line.split_once(attribute).map(|(_, r)| r) else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                continue;
            };
            if let Some(value) = rest[1..].split(quote).next()
                && !value.is_empty()
            {
                found.push(value.to_string());
            }
        }
    }
    found
}

#[test]
fn every_reference_directive_in_the_corpus_is_extracted() {
    // A18. The never-drop guarantee is a claim about phase two *and* about
    // the front end: a reference nothing emits reaches no bucket at all, and
    // no per-reason tally can show its absence.
    let corpus = Path::new("corpus/typescript/vue-core");
    if !corpus_present(corpus) {
        return;
    }
    let mut checked = 0usize;
    for path in source_files::<TsLang>(corpus).expect("walking the corpus") {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let expected = directives_in(&source);
        if expected.is_empty() {
            continue;
        }
        let facts = extract(Dialect::TypeScript, &rel, &source);
        for specifier in expected {
            assert!(
                facts
                    .header
                    .imports
                    .iter()
                    .any(|i| i.specifier.as_deref() == Some(specifier.as_str())),
                "{rel}: `{specifier}` is written in the source and named by no reference",
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "the corpus carries no directive, so this proves nothing — say so \
         rather than letting a vacuous pass stand in for a measurement",
    );
}
