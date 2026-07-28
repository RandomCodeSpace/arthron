//! Every committed baseline is gated by a test, and no two of them measure the
//! same thing.
//!
//! The ratchets themselves live beside the corpus acceptance for each track —
//! `tests/corpus.rs` for Go, `tests/java_corpus.rs`, `tests/corpus_ecma.rs`,
//! `tests/corpus_python.rs`, `tests/php_corpus.rs`, `tests/ruby_corpus.rs`,
//! `tests/corpus_rust.rs`, `tests/kotlin_corpus.rs`, `tests/scala_corpus.rs`,
//! `tests/csharp_corpus.rs`, `tests/swift_corpus.rs`, `tests/cpp_corpus.rs`,
//! `tests/bash_corpus.rs`, `tests/hcl_corpus.rs`, `tests/lua_corpus.rs`,
//! `tests/dart_corpus.rs`, `tests/haskell_corpus.rs`, `tests/elixir_corpus.rs`,
//! and `tests/probes.rs` for
//! the probe pin — because
//! each of them measures with its own track's entry point. That spread has one
//! failure mode: a baseline lands in `baselines/` and nothing compares against
//! it, which looks exactly like a passing gate and is the absence of one.
//!
//! This file closes it, and is worth reading for how. It reads no corpus and
//! runs everywhere, including the CI job where `corpus/` is absent and every
//! ratchet skips — so it catches a baseline that no test file names at all.
//!
//! It also catches the other half, which is worse: a baseline whose ratchet
//! exists and never runs. Every file in the right-hand column below skips
//! without a corpus, and the corpus is private, so `cargo test` in CI proves
//! nothing about any of these numbers. The job that does is
//! `.github/workflows/gate.yml`, the one place the corpus is fetched and the
//! one place a rate blocks a merge. Adding a baseline therefore takes **two**
//! entries, not one — a `GATED` row here and a step there — and
//! [`every_gated_baseline_has_a_step_in_the_corpus_gate_workflow`] is what
//! makes forgetting the second fail, in a test that itself needs no corpus.

use std::collections::BTreeSet;
use std::path::Path;

use arthron::gate::{FORMAT, parse_baseline};
use arthron::model::Lang;

/// Every committed baseline, and the test file that compares a scan against
/// it. Add a row here in the same commit that adds the baseline — the test
/// below is what makes forgetting one fail.
///
/// A row here is *not* enforcement. The driver it names skips whenever
/// `corpus/` is absent, which is every CI run; the step in
/// `.github/workflows/gate.yml` is what makes the number block a merge. Both
/// are required, and both are checked below.
const GATED: &[(&str, &str)] = &[
    ("baselines/go-codeiq.toml", "tests/corpus.rs"),
    ("baselines/go-caddy.toml", "tests/corpus.rs"),
    ("baselines/go-probes.toml", "tests/probes.rs"),
    ("baselines/java-commons-lang.toml", "tests/java_corpus.rs"),
    ("baselines/java-gson.toml", "tests/java_corpus.rs"),
    ("baselines/javascript-fastify.toml", "tests/corpus_ecma.rs"),
    ("baselines/javascript-express.toml", "tests/corpus_ecma.rs"),
    ("baselines/typescript-vue-core.toml", "tests/corpus_ecma.rs"),
    ("baselines/typescript-zod.toml", "tests/corpus_ecma.rs"),
    ("baselines/python-django.toml", "tests/corpus_python.rs"),
    ("baselines/python-flask.toml", "tests/corpus_python.rs"),
    // Tier 2. The file format is the same and the comparison is the same;
    // what the numbers mean is not — these rates are over each track's
    // imports.
    ("baselines/php-guzzle.toml", "tests/php_corpus.rs"),
    ("baselines/ruby-rack.toml", "tests/ruby_corpus.rs"),
    ("baselines/rust-ripgrep.toml", "tests/corpus_rust.rs"),
    ("baselines/kotlin-okio.toml", "tests/kotlin_corpus.rs"),
    ("baselines/scala-upickle.toml", "tests/scala_corpus.rs"),
    ("baselines/csharp-serilog.toml", "tests/csharp_corpus.rs"),
    ("baselines/swift-alamofire.toml", "tests/swift_corpus.rs"),
    ("baselines/cpp-fmt.toml", "tests/cpp_corpus.rs"),
    // Best-effort tier 2: the same mechanism over a denominator of six. The
    // corpus was vendored because none of its `source` targets is a literal
    // path, so this ratchet holds a rate of 0.0% and the drift checks on
    // `external` and `local_binding` — both zero — are what make it
    // un-gameable. See `tests/bash_corpus.rs` for the argument.
    ("baselines/bash-bats-core.toml", "tests/bash_corpus.rs"),
    // Best effort, which is a statement about how much of the language the
    // track reads and not about how honestly it reports it: HCL has no import
    // statement at all, so its denominator is 24 `module` sources over 65
    // files and the definition census beside it carries the weight.
    (
        "baselines/hcl-terraform-aws-vpc.toml",
        "tests/hcl_corpus.rs",
    ),
    ("baselines/lua-busted.toml", "tests/lua_corpus.rs"),
    // Tier 2, best effort: definitions, structure and the URIs the library
    // directives name — no `show`/`hide` combinator is a reference, so this
    // denominator is smaller than a full tier-2 track's by design.
    ("baselines/dart-collection.toml", "tests/dart_corpus.rs"),
    ("baselines/haskell-aeson.toml", "tests/haskell_corpus.rs"),
    ("baselines/elixir-plug.toml", "tests/elixir_corpus.rs"),
];

#[test]
fn every_committed_baseline_is_gated_by_a_test() {
    let mut on_disk = BTreeSet::new();
    for entry in std::fs::read_dir("baselines").expect("baselines/ is committed") {
        let path = entry.expect("a directory entry").path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            on_disk.insert(path.to_string_lossy().replace('\\', "/"));
        }
    }
    let listed: BTreeSet<String> = GATED.iter().map(|(path, _)| (*path).to_string()).collect();
    assert_eq!(
        on_disk, listed,
        "a baseline nothing compares against is the absence of a gate, not a passing one; \
         add it to GATED and to the test file that measures its corpus",
    );
    for (path, driver) in GATED {
        assert!(
            Path::new(driver).is_file(),
            "{path} names {driver}, which does not exist",
        );
    }
}

#[test]
fn no_two_baselines_measure_the_same_corpus() {
    // Rates are per language and never aggregated, and per corpus for the same
    // reason: two files holding one repository's counts would drift apart, and
    // whichever was compared first would decide the verdict.
    let mut corpora = BTreeSet::new();
    for (path, _) in GATED {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(
            baseline.format, FORMAT,
            "{path} is a format this build cannot read"
        );
        assert!(
            Lang::ALL.iter().any(|l| l.name() == baseline.language),
            "{path} names language `{}`, which no variant carries",
            baseline.language,
        );
        assert!(
            baseline.counts.total() > 0,
            "{path} counts no references at all, and would bless any scan",
        );
        assert!(
            corpora.insert(baseline.corpus.clone()),
            "{path} measures {}, which another baseline already measures",
            baseline.corpus,
        );
    }
    assert_eq!(corpora.len(), GATED.len());
}

/// The corpus-gate workflow, read as text.
///
/// A test that compiled the file with a YAML parser would need a YAML parser,
/// and the crate has no reason to carry one. The steps it checks are a fixed
/// shape — one `run:` line per gate, with the three arguments below — so the
/// tokens are read directly and the shape itself is asserted: a step header
/// this parser does not turn into a triple fails the test rather than
/// silently shrinking the set it compares.
const GATE_WORKFLOW: &str = ".github/workflows/gate.yml";

/// One parsed `arthron gate` invocation: corpus path, `--language`, `--baseline`.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct GateStep {
    corpus: String,
    language: String,
    baseline: String,
}

/// Every `arthron gate` invocation in [`GATE_WORKFLOW`], and the number of
/// step headers they were parsed out of.
fn gate_steps() -> (Vec<GateStep>, usize) {
    let text = std::fs::read_to_string(GATE_WORKFLOW)
        .unwrap_or_else(|e| panic!("reading {GATE_WORKFLOW}: {e}"));
    let headers = text
        .lines()
        .filter(|l| l.trim_start().starts_with("- name: Gate "))
        .count();
    let mut steps = Vec::new();
    for line in text.lines() {
        let Some((_, invocation)) = line.split_once("arthron gate ") else {
            continue;
        };
        let words: Vec<&str> = invocation.split_whitespace().collect();
        let flag = |name: &str| {
            words
                .iter()
                .position(|w| *w == name)
                .and_then(|i| words.get(i + 1))
                .map(|w| (*w).to_string())
        };
        let (Some(corpus), Some(language), Some(baseline)) = (
            words.first().map(|w| (*w).to_string()),
            flag("--language"),
            flag("--baseline"),
        ) else {
            panic!("{GATE_WORKFLOW}: cannot read a gate invocation from `{line}`");
        };
        steps.push(GateStep {
            corpus,
            language,
            baseline,
        });
    }
    (steps, headers)
}

#[test]
fn every_gated_baseline_has_a_step_in_the_corpus_gate_workflow() {
    // The gap this closes: `cargo test` skips every corpus ratchet when
    // `corpus/` is absent, and it is absent in `.github/workflows/ci.yml`
    // because the corpus is private. A baseline with a ratchet but no step in
    // the gate workflow is therefore gated by nothing at all, and looks
    // exactly like a passing gate. This test reads no corpus, so it is one of
    // the things that does run in CI.
    let (steps, headers) = gate_steps();
    assert_eq!(
        steps.len(),
        headers,
        "{GATE_WORKFLOW} has {headers} gate steps and {} readable invocations;          a step this test cannot read is a step it cannot check",
        steps.len(),
    );

    let in_workflow: BTreeSet<String> = steps.iter().map(|s| s.baseline.clone()).collect();
    assert_eq!(
        in_workflow.len(),
        steps.len(),
        "{GATE_WORKFLOW} gates one baseline twice: {steps:?}",
    );
    let listed: BTreeSet<String> = GATED.iter().map(|(path, _)| (*path).to_string()).collect();
    assert_eq!(
        listed, in_workflow,
        "a baseline with no step in {GATE_WORKFLOW} is gated by nothing: `cargo test` skips          its ratchet wherever corpus/ is absent, which is every run of ci.yml",
    );
}

#[test]
fn every_gate_step_names_the_corpus_and_language_its_baseline_records() {
    // A step that points at the wrong tree, or passes the wrong `--language`,
    // is a step that measures something the baseline does not describe — and
    // it would pass or fail for reasons that have nothing to do with the
    // number being gated.
    let (steps, _) = gate_steps();
    for step in &steps {
        let text = std::fs::read_to_string(&step.baseline)
            .unwrap_or_else(|e| panic!("{GATE_WORKFLOW} names {}: {e}", step.baseline));
        let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{}: {e}", step.baseline));
        assert_eq!(
            step.language, baseline.language,
            "{GATE_WORKFLOW} gates {} as `{}`, which the baseline does not record",
            step.baseline, step.language,
        );
        assert_eq!(
            step.corpus, baseline.corpus,
            "{GATE_WORKFLOW} scans {} for {}, whose provenance is {}",
            step.corpus, step.baseline, baseline.corpus,
        );
    }
}
