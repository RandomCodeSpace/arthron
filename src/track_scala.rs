//! The Scala track. **Live.** Owns `.scala` and `.sc`, at **tier 2**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_scala_with`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for both
//! extensions and the driver runs Scala over every file the walk reaches.
//! Three layers, and the boundary between them is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`resolve`] — the one place a Scala [`crate::Outcome`] is produced.
//!   Every reference ends `Resolved`, `External`, or `Unresolved(reason)`,
//!   and there is no way to express "dropped".
//! - [`lang`] — the [`crate::lang::Language`] impl, the FQN grammar the other
//!   two agree on, and [`lang::ScalaProject`], which is empty on purpose.
//!
//! **There is no `project` module, and that is the finding.** Every other
//! track here reads a manifest before it reads a file, because the language
//! states a name the source does not — Go's module path, Rust's crate roots,
//! Ruby's load path, PHP's PSR-4 prefixes. Scala states it in the source: a
//! file's package is its `package` clause and nothing else. So the measured
//! corpus's 15 source-root names across 47 directories, which mill selects
//! among per Scala version and per platform, do not enter one identity — and
//! the cross-build hazard they create shows up where it belongs, as *several
//! files declaring one FQN*, counted rather than silently merged. All 26 of
//! them: [`crate::lang::Resolver::stores_as_package`] keeps an `object` —
//! a container in the FQN grammar, a declaration in the graph — out of the
//! package nodes, because a package declared by every file under it is not a
//! collision and an `object` written once per build configuration is.
//!
//! # What tier 2 means here, stated so nobody has to infer it
//!
//! Definitions, structure, and imports. **No call edges and no type-use
//! resolution**, and the honest consequence is that the extractor emits no
//! call and no type reference *at all*: a tier-2 language that emitted them
//! un-gated would put references into a denominator nothing in this track
//! resolves, and report tier-1 coverage it has not measured. `class C extends
//! Base` is therefore read as part of `C`'s structure and produces no
//! [`crate::model::RefKind::Inherit`] reference.
//!
//! So Scala's gate is an **import-resolution rate**, and it is not comparable
//! with Go's or Java's. One reference per import *selector*: `import a.{B,
//! C}` names two things, `import a._` names one.
//!
//! # The two numbers a reader of the baseline should expect
//!
//! - **`local_binding` is zero, and stays zero.** It is the one bucket the
//!   rate's own definition lets a resolver move references into without
//!   linking anything. Tier 2 emits no expression-level reference, so nothing
//!   here *can* name a local; a non-zero count would mean the contract above
//!   had been widened, and the baseline fails on drift in it.
//! - **`external` is zero, and stays zero.** Scala's platform roots (`java`,
//!   `scala`) and its build's Maven coordinates are both outside this
//!   repository and neither can be named here without guessing — see
//!   [`resolve`] for the argument. Every path that leaves the repository is
//!   [`crate::UnresolvedReason::UnknownPackage`] and counts *against* the
//!   rate. `External` sits outside both rate terms, so a track that mints
//!   none cannot raise its rate by reclassifying.
//!
//! Both are the deliberately expensive answers, and both are what make this
//! rate mean what it says.
//!
//! A baseline is recorded with `arthron gate --rebase`. Scala's rate is
//! Scala's own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod resolve;

/// Scala's registration. **Live**: the track owns `.scala` and `.sc`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for both and the
/// driver runs [`resolve::scan_scala`] over every Scala file the walk
/// reaches.
pub const TRACK: Track = Track {
    name: "scala",
    langs: &[Lang::Scala],
    scan: Some(resolve::scan_scala_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scala_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Scala]);
        assert!(Lang::Scala.owns_extension("scala"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("scala"));
        assert!(TRACK.owns_extension("sc"));
        // Scala reports one rate, under its own language code, and shares an
        // identity space with nobody — Kotlin included.
        assert_eq!(Lang::Scala.domain(), crate::model::Domain::Scala);
    }

    #[test]
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed `.scala` and `.sc` and
        // deliberately left `.sbt` unclaimed; the honest moment to widen that
        // list is a commit that measures the files it adds, and this one does
        // not.
        assert_eq!(Lang::Scala.extensions(), ["scala", "sc"]);
        for unclaimed in ["sbt", "mill", "kt"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
    }

    #[test]
    fn the_registered_extension_list_is_wider_than_the_corpus_measures() {
        // Recorded rather than quietly inherited: the vendored corpus holds
        // 145 `.scala` files and **no** `.sc` file, so the `.sc` claim rides
        // on the registration commit's grammar check and not on a
        // measurement of this track reading one. It is kept because
        // narrowing a committed extension partition would leave `.sc` owned
        // by nobody, which is a bigger claim than leaving it here.
        assert!(Lang::Scala.extensions().contains(&"sc"));
    }
}
