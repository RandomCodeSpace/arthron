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
//! and `tests/probes.rs`, `tests/java_probes.rs`, `tests/python_probes.rs`,
//! `tests/javascript_probes.rs` and `tests/typescript_probes.rs` for the five
//! probe pins — because
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
    ("baselines/java-probes.toml", "tests/java_probes.rs"),
    ("baselines/javascript-fastify.toml", "tests/corpus_ecma.rs"),
    ("baselines/javascript-express.toml", "tests/corpus_ecma.rs"),
    (
        "baselines/javascript-probes.toml",
        "tests/javascript_probes.rs",
    ),
    ("baselines/typescript-vue-core.toml", "tests/corpus_ecma.rs"),
    ("baselines/typescript-zod.toml", "tests/corpus_ecma.rs"),
    (
        "baselines/typescript-probes.toml",
        "tests/typescript_probes.rs",
    ),
    ("baselines/python-django.toml", "tests/corpus_python.rs"),
    ("baselines/python-flask.toml", "tests/corpus_python.rs"),
    ("baselines/python-probes.toml", "tests/python_probes.rs"),
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

/// A YAML line that carries something: blank lines and whole-line comments
/// are dropped, so a comment can neither hide a key nor invent one, and what
/// is left is `(indentation, content)`.
fn significant(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with('#')
        })
        .map(|l| (l.len() - l.trim_start().len(), l.trim().to_string()))
        .collect()
}

/// `key: value` from a mapping line, with a leading `- ` list marker removed.
///
/// `None` for anything that is not a key — a block-scalar continuation line,
/// a bare sequence item — so a `run:` body cannot be mistaken for a step key.
fn split_key(line: &str) -> Option<(String, String)> {
    let line = line.strip_prefix("- ").unwrap_or(line);
    let (key, rest) = line.split_once(':')?;
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    Some((key.to_string(), rest.trim().to_string()))
}

/// One step of the gate job.
struct WorkflowStep {
    /// Its `name:`, or the empty string for a step that declares none.
    name: String,
    /// Its own keys — the header's, plus every mapping line one level in.
    /// Nested values (`with:`, a `run: |` body) are not keys and are not here.
    keys: Vec<(String, String)>,
    /// The whole step, comments and blank lines removed, as one string. What
    /// the soft-failure scan reads, because a `|| true` can sit in a block
    /// scalar where no key parser would find it.
    block: String,
}

/// The corpus-gate workflow, read as text: its triggers, the gate job's own
/// keys, and the job's steps.
struct GateJob {
    /// Everything under the workflow's `on:` key, as `(indentation, content)`.
    ///
    /// The indentation is load-bearing and was not kept once. `pull_request:`
    /// and `pull_request:` with `branches: [no-such-branch]` nested under it
    /// are the same line; what tells them apart is whether the line after it
    /// is indented one level further in. See
    /// [`the_corpus_gate_job_itself_has_no_soft_failure_path`].
    triggers: Vec<(usize, String)>,
    /// The job's own keys — `name`, `runs-on`, `timeout-minutes`, `steps`.
    keys: Vec<(String, String)>,
    steps: Vec<WorkflowStep>,
}

/// Read [`GATE_WORKFLOW`] into a [`GateJob`].
///
/// Indentation-driven rather than YAML-parsed, for the reason the header
/// gives: the crate has no cause to carry a YAML parser. The shape is
/// asserted rather than assumed at every step — a file this reader cannot
/// split into a job with steps panics here, instead of yielding an empty set
/// that every test below would pass against.
fn gate_job() -> GateJob {
    let text = std::fs::read_to_string(GATE_WORKFLOW)
        .unwrap_or_else(|e| panic!("reading {GATE_WORKFLOW}: {e}"));
    let lines = significant(&text);

    let on = lines
        .iter()
        .position(|(indent, content)| *indent == 0 && content == "on:")
        .unwrap_or_else(|| panic!("{GATE_WORKFLOW}: no `on:` block, so nothing triggers it"));
    let triggers: Vec<(usize, String)> = lines[on + 1..]
        .iter()
        .take_while(|(indent, _)| *indent > 0)
        .cloned()
        .collect();

    let jobs = lines
        .iter()
        .position(|(indent, content)| *indent == 0 && content == "jobs:")
        .unwrap_or_else(|| panic!("{GATE_WORKFLOW}: no `jobs:` block"));
    let (job_indent, header) = lines
        .get(jobs + 1)
        .unwrap_or_else(|| panic!("{GATE_WORKFLOW}: `jobs:` declares nothing"));
    assert_eq!(
        header, "gates:",
        "{GATE_WORKFLOW}: the first job is not `gates:`; this reader checks that one",
    );
    let job_indent = *job_indent;
    let body: Vec<&(usize, String)> = lines[jobs + 2..]
        .iter()
        .take_while(|(indent, _)| *indent > job_indent)
        .collect();
    assert!(!body.is_empty(), "{GATE_WORKFLOW}: the gates job is empty");

    let key_indent = body[0].0;
    let keys: Vec<(String, String)> = body
        .iter()
        .filter(|(indent, _)| *indent == key_indent)
        .filter_map(|(_, content)| split_key(content))
        .collect();

    let steps_at = body
        .iter()
        .position(|(indent, content)| *indent == key_indent && content == "steps:")
        .unwrap_or_else(|| panic!("{GATE_WORKFLOW}: the gates job declares no `steps:`"));
    let step_lines: Vec<&(usize, String)> = body[steps_at + 1..]
        .iter()
        .take_while(|(indent, _)| *indent > key_indent)
        .copied()
        .collect();
    assert!(!step_lines.is_empty(), "{GATE_WORKFLOW}: `steps:` is empty");
    let step_indent = step_lines[0].0;
    assert!(
        step_lines[0].1.starts_with("- "),
        "{GATE_WORKFLOW}: `steps:` does not begin with a list item",
    );

    let mut steps: Vec<WorkflowStep> = Vec::new();
    let mut current: Vec<&(usize, String)> = Vec::new();
    let finish = |block: &mut Vec<&(usize, String)>, out: &mut Vec<WorkflowStep>| {
        if block.is_empty() {
            return;
        }
        let keys: Vec<(String, String)> = block
            .iter()
            .filter(|(indent, _)| *indent <= step_indent + 2)
            .filter_map(|(_, content)| split_key(content))
            .collect();
        let name = keys
            .iter()
            .find(|(k, _)| k == "name")
            .map_or(String::new(), |(_, v)| v.clone());
        let text: Vec<&str> = block.iter().map(|(_, c)| c.as_str()).collect();
        out.push(WorkflowStep {
            name,
            keys,
            block: text.join("\n"),
        });
        block.clear();
    };
    for line in step_lines {
        if line.0 == step_indent && line.1.starts_with("- ") {
            finish(&mut current, &mut steps);
        }
        current.push(line);
    }
    finish(&mut current, &mut steps);
    assert!(!steps.is_empty(), "{GATE_WORKFLOW}: no step was read");

    GateJob {
        triggers,
        keys,
        steps,
    }
}

/// Every `arthron gate` invocation in [`GATE_WORKFLOW`], and the number of
/// gate steps they were parsed out of.
fn gate_steps() -> (Vec<GateStep>, usize) {
    let job = gate_job();
    let headers = job
        .steps
        .iter()
        .filter(|s| s.name.starts_with("Gate "))
        .count();
    let mut steps = Vec::new();
    for step in &job.steps {
        let Some((_, run)) = step.keys.iter().find(|(k, _)| k == "run") else {
            continue;
        };
        let Some((_, invocation)) = run.split_once("arthron gate ") else {
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
            panic!("{GATE_WORKFLOW}: cannot read a gate invocation from `{run}`");
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

/// The tokens that make a step, or a job, unable to fail.
///
/// Read as substrings of the step's text rather than as YAML keys on purpose:
/// `continue-on-error` is a key, `|| true` is not, and both end the same way
/// — a red command and a green check. Whole-line comments are already gone by
/// the time this is applied, so prose about `|| true` costs nothing.
const SOFT_FAILURE: &[&str] = &[
    "continue-on-error",
    "|| true",
    "|| :",
    "|| exit 0",
    "; true",
    "&& true",
    "set +e",
    "exit 0",
];

/// The one shape a gate step's command may have.
///
/// Anything else is a wrapper, and a wrapper is where an exit status goes to
/// be discarded — `bash -c '… || true'`, a `set +e` above it, a `for` loop
/// whose status is the last iteration's.
const GATE_COMMAND: &str = "./target/release/arthron gate ";

#[test]
fn no_gate_step_can_pass_without_measuring() {
    // The hole this closes, found by defeating the gate rather than by
    // reading it: `every_gated_baseline_has_a_step_in_the_corpus_gate_workflow`
    // counts steps, and a step carrying `continue-on-error: true` is still a
    // step. Four baselines went on "passing" with their step's effect
    // removed. A gate that cannot fail is not a gate, and counting is no way
    // to tell the difference — so every gate step is checked for the three
    // ways it could be neutralised: the key, the shell, and an `if:` that
    // decides it does not apply today.
    let job = gate_job();
    let gates: Vec<&WorkflowStep> = job
        .steps
        .iter()
        .filter(|s| s.name.starts_with("Gate "))
        .collect();
    assert_eq!(
        gates.len(),
        GATED.len(),
        "{GATE_WORKFLOW} has {} gate steps for {} gated baselines",
        gates.len(),
        GATED.len(),
    );

    for step in &gates {
        for token in SOFT_FAILURE {
            assert!(
                !step.block.contains(token),
                "{GATE_WORKFLOW}: step `{}` carries `{token}`, so a regression it \
                 measures cannot fail the job — which is indistinguishable from a pass",
                step.name,
            );
        }
        // `if:` is the quiet one: the step stays in the list, the job stays
        // green, and the step is skipped. A gate that can decide it does not
        // apply is a gate that can decide it never applies.
        assert!(
            !step.keys.iter().any(|(k, _)| k == "if"),
            "{GATE_WORKFLOW}: step `{}` is conditional, so it can be skipped without \
             anything going red",
            step.name,
        );
        let (_, run) = step
            .keys
            .iter()
            .find(|(k, _)| k == "run")
            .unwrap_or_else(|| panic!("{GATE_WORKFLOW}: step `{}` runs nothing", step.name));
        assert!(
            run.starts_with(GATE_COMMAND),
            "{GATE_WORKFLOW}: step `{}` runs `{run}`, not `{GATE_COMMAND}…`; the job's \
             verdict is the command's exit status and a wrapper is where that gets lost",
            step.name,
        );
        // And exactly the two flags a comparison needs. The fourth way a gate
        // step passes without measuring is not a wrapper, an `if:` or a soft
        // failure — it is the command's own `--rebase`, which `src/main.rs`
        // takes before `evaluate` is ever called: the step overwrites the
        // baseline it exists to enforce with whatever this build measured,
        // prints what it wrote, and exits 0. Against a baseline holding
        // `resolved = 999999` the same step exits 1 without the flag and 0
        // with it. Twenty-nine of those is a corpus gate that self-approves
        // every regression it sees, forever, in green.
        //
        // An allow-list, not a deny-list: `--rebase` is the flag that does it
        // today, and the next one has not been written yet.
        let flags: Vec<&str> = run
            .split_whitespace()
            .filter(|w| w.starts_with("--"))
            .collect();
        assert_eq!(
            flags,
            ["--language", "--baseline"],
            "{GATE_WORKFLOW}: step `{}` passes {flags:?}; a gate step names a corpus, a \
             language and a baseline, and does nothing else to the baseline",
            step.name,
        );
    }
}

#[test]
fn the_corpus_gate_job_itself_has_no_soft_failure_path() {
    // The other half. Every step can be strict and the job still not block a
    // merge: `continue-on-error` on the job, an `if:` on the job, a step
    // earlier in the list that swallows the corpus checkout's failure, or a
    // workflow that never runs on a pull request at all.
    let job = gate_job();

    for (key, value) in &job.keys {
        assert_ne!(
            key, "continue-on-error",
            "{GATE_WORKFLOW}: the gates job is `continue-on-error: {value}`, so no step \
             in it can fail the run",
        );
        assert_ne!(
            key, "if",
            "{GATE_WORKFLOW}: the gates job is conditional on `{value}`, so it can be \
             skipped without anything going red",
        );
    }

    // Not only the gate steps: a `continue-on-error` on the corpus checkout
    // would hand every gate an empty tree, and `arthron gate` exits 2 for
    // "nothing measured" precisely so that cannot be read as a pass — but the
    // key has no legitimate use anywhere in this job, so none is allowed.
    for step in &job.steps {
        assert!(
            !step.block.contains("continue-on-error"),
            "{GATE_WORKFLOW}: step `{}` carries continue-on-error",
            step.name,
        );
    }

    // The job's name is the branch-protection context. Renaming it does not
    // fail anything — it silently drops the required check on main, which is
    // the same failure mode as a step that cannot fail, one level up.
    assert_eq!(
        job.keys
            .iter()
            .find(|(k, _)| k == "name")
            .map(|(_, v)| v.as_str()),
        Some("corpus gates"),
        "{GATE_WORKFLOW}: the job name is the branch-protection context",
    );

    // And it has to run where a merge is decided. A workflow that triggers
    // only on `push` reports the regression after it has landed.
    //
    // Unqualified, which is the half a `starts_with` could not see. A
    // `pull_request:` key carrying `branches: [no-such-branch]` or
    // `paths: ['docs/**']` is still a `pull_request` trigger by that reading,
    // and still never runs the gate on a pull request that changes code —
    // the same failure the comment above describes, reached from the other
    // side. The filter is a line of its own, nested one level in, so what
    // separates a real trigger from a decorative one is whether anything is
    // nested under the key at all.
    let at = job
        .triggers
        .iter()
        .position(|(_, content)| content == "pull_request:")
        .unwrap_or_else(|| {
            panic!(
                "{GATE_WORKFLOW}: no bare `pull_request:` trigger, so a red gate blocks \
                 nothing: {:?}",
                job.triggers,
            )
        });
    let (indent, _) = job.triggers[at];
    assert!(
        job.triggers[at + 1..]
            .first()
            .is_none_or(|(next, _)| *next <= indent),
        "{GATE_WORKFLOW}: `pull_request` is qualified by `{}`, so the gate can be \
         filtered off the pull requests it exists to block",
        job.triggers[at + 1].1,
    );
}

/// The one command the gate job may run the suite with.
///
/// Exact, and an allow-list for the reason a gate step's flags are: `--test
/// corpus`, a test-name filter or an `--exclude` would leave this step in the
/// list, green, running a subset — which is indistinguishable from running all
/// of it, and is how one census stops executing without anything going red.
const SUITE_COMMAND: &str = "cargo test --release --all-features";

/// The environment variable that turns a skipped ratchet into a failed one,
/// spelled as it appears under the step's `env:`.
const REQUIRE_CORPUS: &str = "ARTHRON_REQUIRE_CORPUS: \"1\"";

/// The corpus checkout, identified by where it puts the tree.
const CORPUS_CHECKOUT: &str = "path: corpus";

/// The skip line a corpus test may no longer print for itself, spelled in two
/// halves so this file does not contain the string it forbids — and so the
/// test below does not report itself as the first offender.
const OWN_SKIP: &str = concat!("SKIP", ": no corpus");

#[test]
fn the_corpus_gate_workflow_runs_the_suite_where_the_corpus_exists() {
    // The hole this closes is the one every other test in this file is one
    // level below: the gate steps could all be strict, all be measured and
    // all be required, and the *definition* censuses would still never run.
    //
    // `arthron gate` compares four integers — resolved, unresolved, external
    // and local_binding. It counts no definitions. The censuses that do live
    // in `cargo test`, and `.github/workflows/ci.yml` runs that with no
    // corpus on purpose, so every one of them returns before measuring and is
    // recorded as a pass. Deleting `DefKind::Method` from the Go, Rust or
    // EcmaScript extractor moved none of the four integers, skipped every
    // census that would have caught it, and was a green pull request.
    //
    // So the suite has to run in the one job that fetches the corpus, and
    // this is the test that makes forgetting it fail — in a test file that
    // itself reads no corpus, so it runs everywhere.
    let job = gate_job();
    let suites: Vec<usize> = job
        .steps
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            s.keys
                .iter()
                .any(|(k, v)| k == "run" && v.starts_with("cargo test"))
        })
        .map(|(at, _)| at)
        .collect();
    assert_eq!(
        suites.len(),
        1,
        "{GATE_WORKFLOW}: {} steps run `cargo test`; the corpus suite is one step and \
         the job that has a corpus is the only place it means anything",
        suites.len(),
    );
    let at = suites[0];
    let suite = &job.steps[at];

    let (_, run) = suite
        .keys
        .iter()
        .find(|(k, _)| k == "run")
        .expect("the step was selected by its run");
    assert_eq!(
        run, SUITE_COMMAND,
        "{GATE_WORKFLOW}: step `{}` runs `{run}`, not `{SUITE_COMMAND}`; a filter here \
         excludes a census while leaving the step that appears to run it",
        suite.name,
    );

    // Without this the step is still only half a gate. Every corpus test
    // returns early when its corpus is absent — correct on a machine that
    // never fetched the private repository, and a lie here, where a skip
    // means the checkout did not land where the test looked. The variable is
    // what makes `tests/support::missing` fail instead of print.
    assert!(
        suite.block.lines().any(|l| l == REQUIRE_CORPUS),
        "{GATE_WORKFLOW}: step `{}` does not set `{REQUIRE_CORPUS}`, so a corpus that \
         did not check out is 1500 silent skips and a green job",
        suite.name,
    );

    // And it is a gate, so the three ways a gate step is neutralised apply to
    // it unchanged.
    for token in SOFT_FAILURE {
        assert!(
            !suite.block.contains(token),
            "{GATE_WORKFLOW}: step `{}` carries `{token}`",
            suite.name,
        );
    }
    assert!(
        !suite.keys.iter().any(|(k, _)| k == "if"),
        "{GATE_WORKFLOW}: step `{}` is conditional",
        suite.name,
    );

    // Order is part of the claim: this step measures a corpus, so the corpus
    // has to be on disk before it runs. After the checkout, and after the
    // gate steps — the resolution rate is the primary gate and a regression
    // in it must be named by the step that names the corpus.
    let checkout = job
        .steps
        .iter()
        .position(|s| s.block.lines().any(|l| l == CORPUS_CHECKOUT))
        .unwrap_or_else(|| panic!("{GATE_WORKFLOW}: no step checks the corpus out to `corpus/`"));
    assert!(
        checkout < at,
        "{GATE_WORKFLOW}: step `{}` runs before the corpus is checked out",
        suite.name,
    );
}

#[test]
fn every_corpus_skip_goes_through_the_one_guard() {
    // `tests/support::missing` is where `ARTHRON_REQUIRE_CORPUS` is read, so
    // a test file that prints its own skip line and returns is a ratchet the
    // variable cannot reach: it goes on skipping in the gate job, where a
    // skip is the ratchet not running. Every one of them went through a
    // hand-written `println!` until this test existed.
    //
    // What it checks is narrow and worth stating: no test file may print the
    // skip line itself, and a file whose name says it reads a corpus must
    // call the guard. A new file that invents a different message and is not
    // named for a corpus is outside both — the residue, named rather than
    // implied.
    let mut checked = 0;
    for entry in std::fs::read_dir("tests").expect("tests/ is committed") {
        let path = entry.expect("a directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .to_string();
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        assert!(
            !text.contains(OWN_SKIP),
            "tests/{name} prints its own corpus skip; call `support::missing` instead, \
             which is what ARTHRON_REQUIRE_CORPUS turns into a failure",
        );
        if name.contains("corpus") {
            assert!(
                text.contains("support::missing"),
                "tests/{name} is named for a corpus and never calls `support::missing`, \
                 so an absent corpus there is a skip even in the job that fetches one",
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 19,
        "only {checked} corpus test files were found; this test just stopped covering \
         the ones it used to",
    );
}

/// The README's per-language tables, read as text.
///
/// Those tables state the same four counts the baselines hold, plus two
/// columns derived from them. Nothing enforced the equality, so the published
/// numbers went stale behind three separate re-bases while every gate stayed
/// green — a gate compares a scan against a baseline and has no opinion about
/// prose. This is that opinion, and like the rest of this file it reads no
/// corpus, so it runs in the job where every ratchet skips.
const README: &str = "README.md";

/// One rendered table row: the corpus it names and the cells it states.
struct ReadmeRow {
    lang: String,
    corpus: String,
    commit: String,
    resolved: u64,
    external: u64,
    local_binding: u64,
    unresolved: u64,
    rate: String,
    share: String,
}

/// The display name the tables use for a baseline's `language`, which is not
/// always the baseline's own spelling — `cpp` and `csharp` are the two that
/// differ, and they are the reason this is a table rather than a capitalise.
fn display_name(language: &str) -> &'static str {
    match language {
        "go" => "Go",
        "java" => "Java",
        "javascript" => "JavaScript",
        "typescript" => "TypeScript",
        "python" => "Python",
        "cpp" => "C++",
        "csharp" => "C#",
        "kotlin" => "Kotlin",
        "swift" => "Swift",
        "ruby" => "Ruby",
        "php" => "PHP",
        "rust" => "Rust",
        "scala" => "Scala",
        "dart" => "Dart",
        "elixir" => "Elixir",
        "haskell" => "Haskell",
        "lua" => "Lua",
        "bash" => "Bash",
        "hcl" => "HCL",
        other => panic!("no README display name for language `{other}`"),
    }
}

/// `12,345` — the tables group thousands and the baselines do not.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One percentage to a single decimal place, as the tables render it.
///
/// `None` for a zero denominator rather than a printed `0.0%`: `bats-core`
/// resolves nothing and its rate is a real `0.0%` over six references, but a
/// baseline with no references *at all* has no rate, and printing one for it
/// would be the class of claim this file exists to refuse.
fn percent(numerator: u64, denominator: u64) -> Option<String> {
    if denominator == 0 {
        return None;
    }
    let tenths = ((numerator as f64) * 1000.0 / (denominator as f64)).round() / 10.0;
    Some(format!("{tenths:.1}%"))
}

/// Every table row in [`README`], parsed from the eight-cell shape both
/// tables share.
///
/// A row this parser does not recognise is invisible here, so the caller
/// asserts the count against the number of committed baselines — that is what
/// stops a reformatted table from silently emptying the comparison.
fn readme_rows() -> Vec<ReadmeRow> {
    let text = std::fs::read_to_string(README).expect("README.md is committed");
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('|') || !line.ends_with('|') {
            continue;
        }
        let cells: Vec<&str> = line
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim())
            .collect();
        if cells.len() != 8 {
            continue;
        }
        // The provenance cell is `` `corpus` `commit` ``.
        let provenance: Vec<&str> = cells[1].split_whitespace().collect();
        if provenance.len() != 2 {
            continue;
        }
        let count = |c: &str| c.replace(',', "").parse::<u64>();
        let (Ok(resolved), Ok(external), Ok(local_binding), Ok(unresolved)) = (
            count(cells[2]),
            count(cells[3]),
            count(cells[4]),
            count(cells[5]),
        ) else {
            continue;
        };
        rows.push(ReadmeRow {
            lang: cells[0].to_string(),
            corpus: provenance[0].trim_matches('`').to_string(),
            commit: provenance[1].trim_matches('`').to_string(),
            resolved,
            external,
            local_binding,
            unresolved,
            // `**69.5%**`, or `**100.0%** †` where the dagger marks a
            // synthetic corpus and is not part of the number.
            rate: cells[6].replace("**", "").replace('†', "").trim().to_string(),
            share: cells[7].to_string(),
        });
    }
    rows
}

#[test]
fn every_readme_table_row_matches_its_baseline() {
    let rows = readme_rows();
    assert_eq!(
        rows.len(),
        GATED.len(),
        "the README states {} table rows and {} baselines are committed; every baseline \
         gets a row and no row may name anything else",
        rows.len(),
        GATED.len(),
    );

    for (path, _) in GATED {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
        let corpus = baseline
            .corpus
            .rsplit('/')
            .next()
            .expect("a corpus path has a last component");
        let lang = display_name(&baseline.language);

        let row = rows
            .iter()
            .find(|r| r.lang == lang && r.corpus == corpus)
            .unwrap_or_else(|| {
                panic!("{path} has no README row: no table states `{corpus}` under {lang}")
            });

        for (column, stated, held) in [
            ("resolved", row.resolved, baseline.counts.resolved),
            ("external", row.external, baseline.counts.external),
            (
                "local-binding",
                row.local_binding,
                baseline.counts.local_binding,
            ),
            ("unresolved", row.unresolved, baseline.counts.unresolved),
        ] {
            assert_eq!(
                stated,
                held,
                "README row `{corpus}` states {column} {}, {path} holds {}; re-render the \
                 row from the baseline rather than editing it",
                grouped(stated),
                grouped(held),
            );
        }

        assert_eq!(
            row.commit, baseline.commit,
            "README row `{corpus}` pins commit {} and {path} records {}",
            row.commit, baseline.commit,
        );

        // The two derived columns are recomputed here, not trusted.
        let denominator = baseline.counts.resolved + baseline.counts.unresolved;
        let emitted = denominator + baseline.counts.external + baseline.counts.local_binding;
        let rate = percent(baseline.counts.resolved, denominator)
            .unwrap_or_else(|| panic!("{path} has no rate: resolved + unresolved is zero"));
        let share = percent(denominator, emitted)
            .unwrap_or_else(|| panic!("{path} has no share: it emitted no references"));
        assert_eq!(
            row.rate, rate,
            "README row `{corpus}` states rate {} and {path} derives {rate}",
            row.rate,
        );
        assert_eq!(
            row.share, share,
            "README row `{corpus}` states a rate denominator share of {} and {path} derives \
             {share}",
            row.share,
        );
    }
}

#[test]
fn every_published_rate_carries_its_denominator_share() {
    // The README commits in prose to "every table here carries it as a
    // column". That is checked as a shape rather than trusted as a sentence:
    // a rate published without its share is exactly the misreading the
    // accounting exists to prevent, so a table that grows a rate column and
    // not a share column fails here.
    let text = std::fs::read_to_string(README).expect("README.md is committed");
    let mut headers = 0;
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("| language | corpus |") {
            continue;
        }
        headers += 1;
        assert!(
            line.contains("| rate |") && line.contains("| rate denom. |"),
            "a per-language table states a rate without a `rate denom.` column: {line}",
        );
    }
    assert_eq!(
        headers, 2,
        "expected the tier-1 and tier-2 tables; found {headers} per-language table headers",
    );

    for row in &readme_rows() {
        assert!(
            row.rate.ends_with('%') && row.share.ends_with('%'),
            "row `{}` does not state both a rate and its share: {} / {}",
            row.corpus,
            row.rate,
            row.share,
        );
    }
}
