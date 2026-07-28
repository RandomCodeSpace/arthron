//! What a corpus test does when the corpus is not on disk.
//!
//! The corpora are private — they live in `RandomCodeSpace/arthron-corpus` and
//! are cloned into `corpus/` (gitignored) — so skipping is the right answer on
//! a machine that never fetched them. Failing there would make an unfetched
//! corpus look like a broken engine.
//!
//! It is the wrong answer in exactly one place, and that place is the reason
//! this module exists. `.github/workflows/ci.yml` runs `cargo test` without a
//! corpus, by design, so every ratchet and every census skips there.
//! `.github/workflows/gate.yml` is the one job that fetches the corpus — and
//! until it ran the suite too, no definition census, no exact reason tally and
//! no store-side count executed in CI at all. Every one of them was a pass
//! recorded for a test that returned before measuring anything, which is the
//! same shape as the gate steps that could not fail: green, and empty.
//!
//! Now that the gate job runs the suite, a *skip* there is the residue of the
//! same hole: it no longer means "nobody fetched the corpus", it means the
//! corpus this test looks for is not where it looked, and the ratchet did not
//! run. `ARTHRON_REQUIRE_CORPUS` is what separates the two readings. The gate
//! job sets it and a missing corpus is a failure; nothing else sets it and a
//! missing corpus stays a skip.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::model::reason_name;

/// Report a corpus that is not on disk: a skip, or — under
/// `ARTHRON_REQUIRE_CORPUS` — a failure.
///
/// Every "no corpus" branch in `tests/` ends here, which is what lets one
/// environment variable reach all of them.
#[track_caller]
pub fn missing(corpus: &Path) {
    assert!(
        std::env::var_os("ARTHRON_REQUIRE_CORPUS").is_none(),
        "no corpus at {} and ARTHRON_REQUIRE_CORPUS is set: this is the job that \
         fetches the corpus, so a skip here is a ratchet that did not run",
        corpus.display(),
    );
    println!("SKIP: no corpus at {} — see README", corpus.display());
}

/// Assert that a corpus's unresolved references carry exactly these reasons.
///
/// Not a floor. A floor — "`AmbiguousOverload` is above zero" — survives any
/// relabelling that leaves one reference behind, and relabelling is free:
/// `arthron gate` compares four integers (`resolved`, `unresolved`,
/// `external`, `local_binding`) and every one of them is identical whichever
/// reason each unresolved reference carries. The baselines record no reason
/// breakdown at all, so the gate is blind to it by construction. Rewriting one
/// arm of the Java resolver moved 9056 of commons-lang's 19093 unresolved
/// references from `AmbiguousOverload` to `NoMatchingDefinition` — "there is
/// no such definition" in place of "there are several and I cannot choose",
/// 47% of the corpus misreported — with the whole suite green.
///
/// The tier-2 tracks pinned their reasons exactly from the start; the tier-1
/// tracks, whose rates reach furthest into a file, had floors. The
/// non-negotiable is that `Unresolved` is stored *with a reason*, which makes
/// the reason the payload — so it is pinned, and a bucket that moves is
/// re-based deliberately rather than discovered later.
///
/// Every test crate compiles its own copy of this module and only the tier-1
/// corpora call this one, hence the `dead_code` allowance.
#[allow(dead_code)]
#[track_caller]
pub fn assert_reasons(corpus: &str, unresolved: &BTreeMap<u8, u64>, want: &[(&str, u64)]) {
    let got: BTreeMap<&str, u64> = unresolved
        .iter()
        .map(|(code, count)| (reason_name(*code), *count))
        .collect();
    let want: BTreeMap<&str, u64> = want.iter().copied().collect();
    assert_eq!(
        got, want,
        "{corpus}: a reason bucket moved. Reasons are pinned exactly because nothing \
         else can see them — the four gated integers do not move when a reference is \
         relabelled, and no baseline records a reason. Re-base this list deliberately.",
    );
}
