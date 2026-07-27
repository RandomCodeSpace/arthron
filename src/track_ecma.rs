//! The EcmaScript track. **Not live.** Owns `.js`, `.mjs`, `.cjs` and `.ts`.
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
//! [`TRACK`] is registered with `scan: None`, so the driver runs nothing and
//! [`crate::registry::Track::owns_extension`] answers `false` for all four
//! extensions. `.d.ts` needs no rule: its extension is `ts`.
//!
//! # Going live
//!
//! Every step happens inside this file or under `src/track_ecma/`. Nothing in
//! `pipeline.rs`, `lib.rs`, `model.rs`, `registry.rs` or another track moves.
//!
//! 1. **Submodules, nested.** `mod extract;`, `mod resolve;` here resolve to
//!    `src/track_ecma/extract.rs` and `src/track_ecma/resolve.rs`; `lib.rs`
//!    already declares `track_ecma`.
//! 2. **Two [`crate::lang::Language`] impls, not one.** The shared driver
//!    tags every stored row with `L::LANG`, so a single impl can only report
//!    a single rate — and this track owes two. Supply `JsLang` and `TsLang`
//!    with the *same* `DOMAIN`, `Header`, `Scope` and `Config` types and the
//!    same extractor and resolver values, differing only in `LANG` and
//!    `extensions()` ([`Lang::extensions`] for `Lang::JavaScript` and for
//!    `Lang::TypeScript` respectively). Teaching the driver a per-file
//!    language instead would work too, and would edit a shared file — which
//!    is exactly the conflict this layout exists to avoid. Prefer the two
//!    impls.
//! 3. **An extractor** implementing [`crate::lang::Extractor`] for both,
//!    parsing with [`crate::sg::SourceTree::parse_javascript`] and
//!    [`crate::sg::SourceTree::parse_typescript`]. It emits
//!    [`crate::model::Definition`] and [`crate::model::Reference`] records
//!    and **never an edge**.
//! 4. **One resolver** implementing [`crate::lang::Resolver`] for both impls:
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
//! 7. **An entry point** with the shape of [`crate::registry::TrackScan`]:
//!    `fn scan_ecma(root, db)`, driving both impls through
//!    [`crate::pipeline::scan`] and returning the later report.
//! 8. **Flip the switch here**: `scan: None` becomes `scan: Some(scan_ecma)`.
//! 9. **Two baselines, never one.** `arthron gate` compares per language; a
//!    baseline file names the language it measures and refuses to be compared
//!    against another's scan.
//!
//! Sharing the store with a live Go track is safe: a scan forgets only files
//! carrying an extension the running track owns, and extension ownership is a
//! partition (see [`Lang::for_extension`]).

use crate::model::Lang;
use crate::registry::Track;

/// The EcmaScript family's registration. `scan: None`: the track owns no
/// file and contributes nothing to a scan until the work above lands.
///
/// `langs` lists two entries because two rates are reported. One track is an
/// implementation fact; two languages is a reporting obligation.
pub const TRACK: Track = Track {
    name: "ecma",
    langs: &[Lang::JavaScript, Lang::TypeScript],
    scan: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecma_is_registered_but_not_live() {
        assert!(!TRACK.is_enabled());
        for ext in ["js", "mjs", "cjs", "ts"] {
            assert!(Lang::for_extension(ext).is_some());
            assert!(!TRACK.owns_extension(ext));
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
