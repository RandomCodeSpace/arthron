//! The EcmaScript track. **Live.** Owns `.js`, `.mjs`, `.cjs` and `.ts`.
//!
//! One track, **two languages**. JavaScript and TypeScript share
//! [`crate::model::Domain::EcmaScript`] because a `.ts` file naming a
//! definition in a `.js` file has to probe an identity that can exist, and
//! they share a resolver family because the linking rules — module specifier
//! resolution, export sets, re-exports — are the same rules. They do **not**
//! share a rate: [`Lang::JavaScript`] and [`Lang::TypeScript`] are separate
//! [`crate::model::Lang`] values with separate storage codes, they get one
//! report line each, and no combined EcmaScript number is ever produced. One
//! number would let a collapse in one of them be masked by the other.
//!
//! [`TRACK`] is registered with `scan: Some(scan_ecma)`, so the driver runs
//! both passes and [`crate::registry::Track::owns_extension`] answers `true`
//! for all four extensions. `.d.ts` needs no rule: its extension is `ts`.
//!
//! # How it went live
//!
//! Every step happened inside this file or under `src/track_ecma/`. Nothing in
//! `pipeline.rs`, `lib.rs`, `model.rs`, `registry.rs` or another track moved.
//!
//! 1. **Submodules, nested.** *Landed.* [`extract`], [`lang`] and the
//!    private `bind` resolve to `src/track_ecma/*.rs`; `lib.rs` already
//!    declares `track_ecma`, so nothing shared moved. `mod resolve;` joins
//!    them the same way.
//! 2. **Two [`crate::lang::Language`] impls, not one.** *Landed* as
//!    [`lang::JsLang`] and [`lang::TsLang`]. The shared driver
//!    tags every stored row with `L::LANG`, so a single impl can only report
//!    a single rate — and this track owes two. Supply `JsLang` and `TsLang`
//!    with the *same* `DOMAIN`, `Header`, `Scope` and `Config` types and the
//!    same extractor and resolver values, differing only in `LANG` and
//!    `extensions()` ([`Lang::extensions`] for `Lang::JavaScript` and for
//!    `Lang::TypeScript` respectively). Teaching the driver a per-file
//!    language instead would work too, and would edit a shared file — which
//!    is exactly the conflict this layout exists to avoid. Prefer the two
//!    impls.
//! 3. **An extractor.** *Landed* as [`extract::JsExtractor`] and
//!    [`extract::TsExtractor`], implementing [`crate::lang::Extractor`] for both,
//!    parsing with [`crate::sg::SourceTree::parse_javascript`] and
//!    [`crate::sg::SourceTree::parse_typescript`]. It emits
//!    [`crate::model::Definition`] and [`crate::model::Reference`] records
//!    and **never an edge**.
//! 4. **One resolver.** *Landed* as [`resolve::EcmaResolver`], implementing
//!    [`crate::lang::Resolver`] for both impls:
//!    the same linking code, so a `.ts` file and a `.js` file resolve against
//!    one symbol table in one identity space. It is the only place an
//!    [`crate::Outcome`] is produced, and it never drops: every reference
//!    ends `Resolved`, `External`, or `Unresolved(reason)`, and a name some
//!    enclosing block binds ends `Unresolved(LocalBinding)` — reported beside
//!    `External`, excluded from both terms of the rate.
//! 5. **Honest reasons.** This family's floor is real and large:
//!    [`crate::UnresolvedReason::NeedsTypeInference`] and
//!    [`crate::UnresolvedReason::NeedsReceiverType`] for member access on an
//!    untyped value, [`crate::UnresolvedReason::DynamicModuleSpecifier`] for
//!    a non-literal `import()`, [`crate::UnresolvedReason::WildcardImport`]
//!    for a star re-export whose source cannot be enumerated. Those are the
//!    measurement. Reclassifying them as `LocalBinding` or `External` would
//!    move them out of the denominator and raise the rate without linking
//!    anything, which is the one way this gate can be cheated — so it is the
//!    one thing review looks for.
//! 6. **A scan order that is a decision, not an accident.** Both languages'
//!    files land in one store and one domain, so whichever runs first is
//!    resolved before the other's definitions exist. The candidate index
//!    wakes the files that probed an identity that later appeared, so the end
//!    state is correct either way; the track still has to say which order it
//!    runs and why, and prove it with a cold-versus-incremental comparison.
//! 7. **An entry point** with the shape of [`crate::registry::TrackScan`].
//!    *Landed* as [`scan_ecma`], driving both impls through
//!    [`crate::pipeline::scan`] and returning the later report.
//! 8. **The switch, here.** *Landed*: `scan: None` became `scan: Some(scan_ecma)`.
//! 9. **Two baselines, never one.** *Landed.* `arthron gate --language`
//!    compares per language; a baseline file names the language it measures
//!    and refuses to be compared against another's scan.
//!
//! Sharing the store with a live Go track is safe: a scan forgets only files
//! carrying an extension the running track owns, and extension ownership is a
//! partition (see [`Lang::for_extension`]).

mod bind;
pub mod extract;
mod globals;
mod json;
pub mod lang;
pub mod project;
pub mod resolve;
pub mod specifier;

use std::path::Path;

use crate::model::Lang;
use crate::pipeline::scan;
use crate::registry::Track;
use crate::store::Report;
use crate::track_ecma::extract::{JsExtractor, TsExtractor};
use crate::track_ecma::lang::{JsLang, TsLang};
use crate::track_ecma::resolve::{JS_RESOLVER, TS_RESOLVER};

/// Scan a repository's JavaScript and TypeScript, in that order.
///
/// **The order is a decision.** Both languages land in one store and one
/// identity space, and the driver's wake set is filtered by the files the
/// *running* language owns — so a reference in a file of language A that
/// probes an identity language B declares later is not re-resolved when B
/// declares it. Whichever runs second therefore resolves against a complete
/// table and whichever runs first does not, for names.
///
/// JavaScript runs first because a `.ts` file naming a definition in a `.js`
/// file is the direction that exists: `allowJs`, and every hand-written
/// `.d.ts` describing a `.js` implementation. The reverse — a `.js` file
/// importing a name out of a `.ts` source — is not something any build does,
/// so it is the direction to give up.
///
/// *Module* identity is not affected either way: a specifier resolves against
/// the file set both languages' configs are built from, so a `.js` file
/// importing a `.ts` file still links to the right module node whichever ran
/// first.
///
/// The returned [`Report`] is the TypeScript pass's, which is the whole
/// report: [`crate::store::Store::report`] tallies every row in the store and
/// keys the tallies by language, so JavaScript's line is already in it and no
/// combined EcmaScript number exists to return.
pub fn scan_ecma(root: &Path, db_path: &Path) -> Result<Report, String> {
    scan_ecma_with(root, db_path, &crate::config::FileFilter::none())
}

/// [`scan_ecma`] under a repository's include/exclude globs. What [`TRACK`]
/// holds.
///
/// One filter for both passes: the repository decides which files exist for
/// this scan, and the two languages of one track must never disagree about
/// that — a `.ts` file excluded from the TypeScript pass but present for the
/// JavaScript one would resolve differently depending on which ran.
pub fn scan_ecma_with(
    root: &Path,
    db_path: &Path,
    filter: &crate::config::FileFilter,
) -> Result<Report, String> {
    scan::<JsLang>(root, db_path, &JsExtractor, &JS_RESOLVER, filter)?;
    scan::<TsLang>(root, db_path, &TsExtractor, &TS_RESOLVER, filter)
}

/// The EcmaScript family's registration.
///
/// `langs` lists two entries because two rates are reported. One track is an
/// implementation fact; two languages is a reporting obligation.
pub const TRACK: Track = Track {
    name: "ecma",
    langs: &[Lang::JavaScript, Lang::TypeScript],
    scan: Some(scan_ecma_with),
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Domain;

    #[test]
    fn ecma_is_live_and_owns_exactly_its_four_extensions() {
        assert!(TRACK.is_enabled());
        for ext in ["js", "mjs", "cjs", "ts"] {
            assert_eq!(
                Lang::for_extension(ext).map(Lang::domain),
                Some(Domain::EcmaScript)
            );
            assert!(TRACK.owns_extension(ext), "`.{ext}` is unowned");
        }
        // `.d.ts` needs no rule of its own: its extension *is* `ts`.
        assert!(TRACK.owns_extension("ts"));
        // And nothing beyond them. `.tsx`/`.jsx` are real EcmaScript code
        // that no `Lang` owns in this build — recorded as a core gap, and
        // reported as `TierTwoLanguage` when a specifier reaches one, never
        // silently claimed here.
        for ext in ["go", "java", "py", "tsx", "jsx", "mts", "cts"] {
            assert!(!TRACK.owns_extension(ext), "ecma claims `.{ext}`");
        }
    }

    #[test]
    fn one_track_carries_two_languages_and_two_rates() {
        assert_eq!(TRACK.langs, [Lang::JavaScript, Lang::TypeScript]);
        // One identity space...
        assert_eq!(Lang::JavaScript.domain(), Lang::TypeScript.domain());
        // ...but two report lines, and no way to spell a combined one.
        assert_ne!(Lang::JavaScript.code(), Lang::TypeScript.code());
    }
}
