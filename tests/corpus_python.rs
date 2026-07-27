//! Resolver acceptance for Python against the django corpus: nothing is
//! dropped, and the measured counts hold the committed baseline.
//!
//! Distinct from `tests/python_corpus.rs`, which is the *extractor*'s
//! acceptance — that one asserts the record-level invariants of one file at a
//! time and never resolves anything. This one runs the whole track and asks
//! the two questions a rate is only worth reading if you can answer:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External`, `LocalBinding` or `Unresolved(reason)`.
//!    The check re-extracts the same files independently and compares totals,
//!    because a resolver that silently dropped its hardest references would
//!    otherwise report a *better* rate for doing less work.
//! 2. **The ratchet.** The counts are compared against `baselines/`
//!    through the same [`arthron::gate::evaluate`] the `arthron gate` command
//!    uses, so a rate regression, or drift in either of the two buckets that
//!    sit outside the rate, fails the build.
//!
//! # Why this is a test and not `arthron gate`
//!
//! It should be `arthron gate corpus/python/django --baseline …`, and today it
//! cannot be, for two reasons in files this track does not own:
//!
//! - `pipeline::scan_repo` runs every live track unconditionally, and the Go
//!   resolver's `config` reads `root/go.mod` before it looks at whether the
//!   walk found any Go at all. A repository that is only Python therefore
//!   fails the whole scan with `reading go.mod: No such file or directory`.
//! - `main.rs` hardcodes `Lang::Go` on both sides of the gate — the baseline's
//!   `language` check and the tally it measures — so there is no way to ask it
//!   for another language's numbers.
//!
//! Both are recorded as core gaps. Until they close, this test is Python's
//! gate: it drives the track's own entry point, the one `scan_repo` would
//! call, and writes its baseline with the product's own renderer so the file
//! is byte-identical to one `--rebase` would produce.
//!
//! Re-base deliberately, exactly as the ratchet requires, with:
//!
//! ```text
//! ARTHRON_PYTHON_REBASE=1 ARTHRON_PYTHON_COMMIT=<sha> \
//!     cargo test --release --test corpus_python
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken resolver.

use std::path::Path;

use arthron::gate::{Baseline, Counts, GateVerdict, evaluate, parse_baseline, render_baseline};
use arthron::model::{Lang, reason_name};
use arthron::store::Store;
use arthron::track_python::extract::extract;
use arthron::track_python::resolve::scan_python;

const CORPUS: &str = "corpus/python/django";
const BASELINE: &str = "baselines/python-django.toml";

#[test]
fn the_python_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_python(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Python.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "python       resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    for (code, count) in &tally.unresolved {
        println!("             {} {count}", reason_name(*code));
    }

    // -- completeness -----------------------------------------------------

    // Independently re-extracted: the same files the scan owned, read again
    // from disk and put through the extractor with no resolver in sight. The
    // scan's four buckets must account for every one of those references and
    // for nothing else.
    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    drop(store);
    assert!(!owned.is_empty(), "the scan owned no file");

    let mut re_extracted = 0u64;
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        re_extracted += extract(rel, &source).refs.len() as u64;
    }

    let accounted =
        measured.resolved + measured.external + measured.local_binding + measured.unresolved;
    assert_eq!(
        accounted,
        re_extracted,
        "{} references were extracted from {} files but {accounted} were accounted for; \
         a resolver that drops a reference reports a better rate for less work",
        re_extracted,
        owned.len(),
    );

    // The four buckets are a partition, so none of them may be the whole of
    // it: a run where everything landed in one bucket accounts for every
    // reference and still measures nothing.
    assert!(measured.resolved > 0, "nothing linked at all");
    assert!(measured.unresolved > 0, "no floor: every reason is empty");
    assert!(
        measured.external > 0,
        "nothing reached outside the repository"
    );
    assert!(
        measured.local_binding > 0,
        "no local binding was recognised"
    );

    // -- the ratchet ------------------------------------------------------

    let baseline_path = Path::new(BASELINE);
    if std::env::var_os("ARTHRON_PYTHON_REBASE").is_some() {
        let previous = std::fs::read_to_string(baseline_path)
            .ok()
            .and_then(|text| parse_baseline(&text).ok());
        let baseline = Baseline {
            format: 1,
            corpus: CORPUS.to_string(),
            // `--commit`'s stand-in: provenance, printed and never verified,
            // so it is carried forward rather than invented when unset.
            commit: std::env::var("ARTHRON_PYTHON_COMMIT")
                .ok()
                .filter(|c| !c.is_empty())
                .or_else(|| previous.map(|b| b.commit))
                .unwrap_or_else(|| "unknown".to_string()),
            language: Lang::Python.name().to_string(),
            counts: measured,
        };
        std::fs::write(baseline_path, render_baseline(&baseline))
            .unwrap_or_else(|e| panic!("writing {BASELINE}: {e}"));
        println!("REBASED {BASELINE}");
        return;
    }

    let text = std::fs::read_to_string(baseline_path).unwrap_or_else(|e| {
        panic!("reading {BASELINE}: {e}; record it with ARTHRON_PYTHON_REBASE=1")
    });
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Python.name(),
        "{BASELINE} measures another language; rates are per language and never aggregated",
    );
    match evaluate(&baseline, &measured) {
        GateVerdict::Pass { improved } => {
            if improved {
                println!("gate: pass — improved on the baseline; re-base to move the ratchet");
            }
        }
        GateVerdict::Fail(failures) => {
            let joined: Vec<String> = failures.iter().map(ToString::to_string).collect();
            panic!("gate: FAIL\n  {}", joined.join("\n  "));
        }
        GateVerdict::Error(e) => panic!("gate: error — {e}"),
    }
}
