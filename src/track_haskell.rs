//! The Haskell track. **Live.** Owns `.hs`, at **tier 2, best effort**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_haskell_with`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `hs` and the
//! driver runs Haskell over every `.hs` file the walk reaches. Four layers,
//! and the boundary between the first two is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`project`] — phase 0: the `hs-source-dirs` roots a module name is
//!   looked up under, and the packages the manifests declare. Haskell states
//!   neither in its source.
//! - [`resolve`] — the one place a Haskell [`crate::Outcome`] is produced.
//!   Every reference ends `Resolved`, `External`, or `Unresolved(reason)`, and
//!   there is no way to express "dropped".
//! - [`lang`] — the [`crate::lang::Language`] impl and the FQN grammar the
//!   other three agree on.
//!
//! # What tier 2, best effort means here, stated so nobody has to infer it
//!
//! Definitions, structure, and imports — what the stock grammar gives
//! cheaply. **No call edges and no type-use resolution**, and the honest
//! consequence is that the extractor emits no call and no type reference *at
//! all*: a tier-2 language that emitted them un-gated would put references
//! into a denominator nothing in this track resolves, and report tier-1
//! coverage it has not measured. `data Value = Object !Object` is read as part
//! of `Value`'s structure and produces no [`crate::model::RefKind::TypeUse`]
//! reference; `instance ToJSON Bool` produces no
//! [`crate::model::RefKind::Inherit`] one.
//!
//! *Best effort* lowers the ambition, never the honesty. Every reference this
//! track does emit is still `Resolved`, `External` or `Unresolved` with a
//! ratified reason, the resolver still never drops, and the three shortfalls
//! that matter — Template Haskell splices, which declare code no parser sees;
//! instance heads, which no name spells; and every arm of a CPP conditional
//! after the first, which the pinned grammar swallows — are recorded in
//! [`extract`] with the counts the corpus pays for them, rather than papered
//! over.
//!
//! So Haskell's gate is an **import-resolution rate**, and it is not
//! comparable with Go's or Java's, or with another tier-2 language's. One
//! reference per `import` declaration: `qualified`, `as` and `hiding` all
//! name one module, and a selector list names values inside it that this
//! track does not resolve.
//!
//! # The two numbers a reader of the baseline should expect
//!
//! - **`local_binding` is zero, and stays zero.** It is the one bucket the
//!   rate's own definition lets a resolver move references into without
//!   linking anything. Tier 2 emits no expression-level reference, so nothing
//!   here *can* name a local — a `where` binding is not even a node — and a
//!   non-zero count would mean the contract above had been widened. The
//!   baseline fails on drift in it.
//! - **`external` is large, and every one of them is a fact rather than an
//!   inference.** A Haskell home module is exactly a file under a declared
//!   `hs-source-dirs` root; this scan enumerates every root and every file, so
//!   an import naming none of them cannot be in this repository. Three guards
//!   in [`resolve`] stand in front of that conclusion — no root at all, a name
//!   this repository declares, and a manifest that names no outside dependency
//!   — and each is a fixture. What the external node is *named* costs
//!   precision and not honesty; [`resolve`] carries that argument.
//!
//! Sharing the store with the other live tracks is safe in both directions: a
//! scan forgets only files carrying an extension the running track owns, and
//! extension ownership is a partition (see
//! [`crate::model::Lang::for_extension`]); the manifest fence is per language,
//! and Haskell's digest covers exactly what phase 0 read.
//!
//! A baseline is recorded with `arthron gate --rebase`. Haskell's rate is
//! Haskell's own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod project;
pub mod resolve;

/// Haskell's registration. **Live**: the track owns `.hs`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for it and the
/// driver runs [`resolve::scan_haskell`] over every Haskell file the walk
/// reaches.
pub const TRACK: Track = Track {
    name: "haskell",
    langs: &[Lang::Haskell],
    scan: Some(resolve::scan_haskell_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haskell_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Haskell]);
        assert!(Lang::Haskell.owns_extension("hs"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("hs"));
        // Haskell reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::Haskell.domain(), crate::model::Domain::Haskell);
        // And it reports at tier 2, so its rate is an import-resolution rate
        // and is never read as a tier-1 one.
        assert_eq!(Lang::Haskell.tier(), 2);
        assert_eq!(Lang::Haskell.rate_scope(), "import resolution");
    }

    #[test]
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed `.hs` and deliberately left
        // `.lhs`, `.hs-boot`, `.hsc` and `.chs` unclaimed; the honest moment
        // to widen that list is a commit that measures the files it adds, and
        // this one does not — the corpus contains none of them.
        assert_eq!(Lang::Haskell.extensions(), ["hs"]);
        for unclaimed in ["lhs", "hs-boot", "hsc", "chs", "cabal"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
    }
}
