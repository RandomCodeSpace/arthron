//! Every committed baseline is gated by a test, and no two of them measure the
//! same thing.
//!
//! The ratchets themselves live beside the corpus acceptance for each track —
//! `tests/corpus.rs` for Go, `tests/java_corpus.rs`, `tests/corpus_ecma.rs`,
//! `tests/corpus_python.rs`, `tests/corpus_rust.rs`, and `tests/probes.rs` for
//! the probe pin — because
//! each of them measures with its own track's entry point. That spread has one
//! failure mode: a baseline lands in `baselines/` and nothing compares against
//! it, which looks exactly like a passing gate and is the absence of one.
//!
//! This file closes it. It reads no corpus and runs everywhere, including the
//! CI job where `corpus/` is absent and every ratchet skips.

use std::collections::BTreeSet;
use std::path::Path;

use arthron::gate::{FORMAT, parse_baseline};
use arthron::model::Lang;

/// Every committed baseline, and the test file that compares a scan against
/// it. Add a row here in the same commit that adds the baseline — the test
/// below is what makes forgetting one fail.
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
    ("baselines/rust-ripgrep.toml", "tests/corpus_rust.rs"),
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
