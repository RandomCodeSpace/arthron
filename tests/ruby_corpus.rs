//! Acceptance for the Ruby track against the rack corpus: nothing is dropped,
//! and the measured counts are the ones the committed baseline was recorded
//! from.
//!
//! Two questions, the same two every tier-1 corpus test asks:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The ratchet.** The counts are compared against
//!    `baselines/ruby-rack.toml` through the same [`arthron::gate::evaluate`]
//!    the `arthron gate` command uses, so a rate regression — or drift in
//!    either of the two buckets that sit outside the rate — fails the build.
//!
//! Beside the ratchet sits the tally itself, restated. rack is pinned and is
//! never edited, so every number below is a fact about this extractor and
//! this resolver reading a fixed 93 files; a change to any of them is a
//! change in what the track *does*, and must arrive as a deliberate edit here
//! and a deliberate `--rebase` beside it, never as a test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/ruby/rack --language ruby \
//!     --baseline baselines/ruby-rack.toml --rebase --commit <sha>
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken track.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Lang, RefKind, reason_name};
use arthron::store::Store;
use arthron::track_ruby::extract::{ImportForm, extract};
use arthron::track_ruby::resolve::scan_ruby;

const CORPUS: &str = "corpus/ruby/rack";
const BASELINE: &str = "baselines/ruby-rack.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
const FILES: usize = 93;
const REFERENCES: u64 = 342;
const REQUIRE_RELATIVE: u64 = 247;
const LOAD_PATH: u64 = 94;
const DYNAMIC: u64 = 1;

#[test]
fn the_ruby_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_ruby(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Ruby.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "ruby         resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    for (code, count) in &tally.unresolved {
        println!("             {} {count}", reason_name(*code));
        reasons.insert(reason_name(*code).to_string(), *count);
    }

    // -- completeness -----------------------------------------------------

    // Independently re-extracted: the same files the scan owned, read again
    // from disk and put through the extractor with no resolver in sight. The
    // scan's buckets must account for every one of those references and for
    // nothing else.
    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    drop(store);
    assert_eq!(owned.len(), FILES, "the scan owned a different file set");

    let mut re_extracted = 0u64;
    let mut forms: BTreeMap<&str, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        let facts = extract(rel, &source);
        re_extracted += facts.refs.len() as u64;
        for r in &facts.refs {
            // The tier-2 contract, checked on real code and not only on a
            // fixture: a call or type reference here would put references
            // into a denominator this track cannot resolve.
            assert_eq!(r.kind, RefKind::Import, "{rel}: {}", r.raw_target);
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
        }
        // An import clause and its reference are paired by span, so a clause
        // with no reference would be a silently dropped import.
        assert_eq!(
            facts.header.imports.len(),
            facts.refs.len(),
            "{rel}: import clauses and import references disagree",
        );
        for spec in &facts.header.imports {
            *forms
                .entry(match spec.form {
                    ImportForm::Relative(_) => "relative",
                    ImportForm::LoadPath(_) => "load-path",
                    ImportForm::Dynamic => "dynamic",
                })
                .or_default() += 1;
        }
        // Every file declares the feature a `require` names, first, whether
        // or not it declares a constant.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no feature",
        );
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
        }
    }
    println!("             forms {forms:?}");
    println!("             defs  {kinds:?}");

    let accounted =
        measured.resolved + measured.external + measured.local_binding + measured.unresolved;
    assert_eq!(
        accounted,
        re_extracted,
        "{re_extracted} references were extracted from {} files but {accounted} were accounted \
         for; a resolver that drops a reference reports a better rate for less work",
        owned.len(),
    );

    // -- the tally, exactly -----------------------------------------------

    assert_eq!(re_extracted, REFERENCES);
    assert_eq!(forms.get("relative").copied(), Some(REQUIRE_RELATIVE));
    assert_eq!(forms.get("load-path").copied(), Some(LOAD_PATH));
    assert_eq!(forms.get("dynamic").copied(), Some(DYNAMIC));

    // Every `require_relative` in rack names a real sibling, and every one of
    // the 43 `autoload`s plus the single `require 'rack'` reaches `lib/`.
    assert_eq!(measured.resolved, 291);
    // The one gem the source requires by a name `rack.gemspec` declares.
    assert_eq!(measured.external, 1);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The bucket that sits outside both rate terms is empty, which is
    // what makes this rate un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    assert_eq!(measured.unresolved, 50);

    // The floor, named. Ruby's standard library is not indexed here, so
    // `require 'time'` is a package outside the repository that was not
    // indexed — and it counts *against* the rate rather than being waved
    // through as external.
    assert_eq!(reasons.get("UnknownPackage").copied(), Some(49));
    // `Rack::Builder.parse_file` ends in `require path`. A specifier built at
    // runtime is never guessed.
    assert_eq!(reasons.get("DynamicModuleSpecifier").copied(), Some(1));
    assert_eq!(
        reasons.len(),
        2,
        "an unexpected reason appeared: {reasons:?}"
    );

    // -- the ratchet ------------------------------------------------------

    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Ruby.name(),
        "{BASELINE} measures another language; rates are per language and never aggregated",
    );
    assert_eq!(
        baseline.corpus, CORPUS,
        "{BASELINE} was recorded from another corpus",
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
