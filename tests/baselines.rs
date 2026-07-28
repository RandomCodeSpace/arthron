//! Every committed baseline is gated by a test, and no two of them measure the
//! same thing.
//!
//! The ratchets themselves live beside the corpus acceptance for each track —
//! `tests/corpus.rs` for Go, `tests/java_corpus.rs`, `tests/corpus_ecma.rs`,
//! `tests/corpus_python.rs`, `tests/php_corpus.rs`, `tests/ruby_corpus.rs`,
//! `tests/corpus_rust.rs`, `tests/kotlin_corpus.rs`, `tests/scala_corpus.rs`,
//! `tests/csharp_corpus.rs`, `tests/swift_corpus.rs`, `tests/cpp_corpus.rs`,
//! `tests/bash_corpus.rs`, `tests/hcl_corpus.rs`, `tests/lua_corpus.rs`,
//! and `tests/probes.rs` for
//! the probe pin — because
//! each of them measures with its own track's entry point. That spread has one
//! failure mode: a baseline lands in `baselines/` and nothing compares against
//! it, which looks exactly like a passing gate and is the absence of one.
//!
//! This file closes half of it, and is worth reading for which half. It reads
//! no corpus and runs everywhere, including the CI job where `corpus/` is
//! absent and every ratchet skips — so it does catch a baseline that no test
//! file names at all.
//!
//! What it does **not** catch is a baseline whose ratchet exists and never
//! runs. Every file in the right-hand column below skips without a corpus,
//! and the corpus is private, so `cargo test` in CI proves nothing about any
//! of these numbers. The job that does is
//! `.github/workflows/gate.yml`, which is the one place the corpus is
//! fetched, and a `GATED` row here says nothing about whether a step there
//! names the same baseline. Adding a baseline therefore takes **two** entries,
//! not one, and only the first of them is checked below.

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
/// are required, and nothing here checks the second.
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
