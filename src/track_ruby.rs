//! The Ruby track. **Live.** Owns `.rb`, at **tier 2**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_ruby`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `rb` and the
//! driver runs Ruby over every `.rb` file the walk reaches. Three layers, and
//! the boundary between them is the project's first non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`project`] — phase 0: where the load path starts, and which gems the
//!   gemspec declares. Ruby states neither in its source.
//! - [`resolve`] — the one place a Ruby [`crate::Outcome`] is produced. Every
//!   reference ends `Resolved`, `External`, or `Unresolved(reason)`, and
//!   there is no way to express "dropped".
//!
//! [`lang`] holds the [`crate::lang::Language`] impl and the FQN grammar the
//! other three agree on.
//!
//! # What tier 2 means here, precisely
//!
//! Definitions, structure, and imports. **No call edges and no type-use
//! resolution**, and the honest consequence is that the extractor emits no
//! call or type reference *at all*: a tier-2 language that emitted them
//! un-gated would put references into a denominator nothing in this track
//! resolves, and report tier-1 coverage it has not measured.
//!
//! So Ruby's gate is an **import-resolution rate**. The reference kinds this
//! track emits are `require`, `require_relative` and `autoload` — module
//! references, one per clause — and the definitions beside them are the
//! structure: modules, classes, methods, constants, attributes, and the
//! *feature* every `.rb` file is.
//!
//! # The two numbers a reader of the baseline should expect
//!
//! - **`local_binding` is zero, and stays zero.** It is the one bucket the
//!   rate's own definition lets a resolver move references into without
//!   linking anything. Tier 2 emits no expression-level reference, so nothing
//!   here *can* name a local; a non-zero count would mean the contract above
//!   had been widened, and the baseline fails on drift in it.
//! - **`external` is small, and comes only from the gemspec.** Ruby's
//!   standard library is not indexed, so `require 'time'` is
//!   [`crate::UnresolvedReason::UnknownPackage`] and counts *against* the
//!   rate. See [`resolve`] for why that is the deliberate answer and not a
//!   missing feature: `External` sits outside both terms, and widening it is
//!   the cheapest way there is to raise a rate with nothing linked.
//!
//! Sharing the store with the other live tracks is safe in both directions: a
//! scan forgets only files carrying an extension the running track owns, and
//! extension ownership is a partition (see [`crate::model::Lang::for_extension`]);
//! the manifest fence is per language, and Ruby's digest covers exactly what
//! phase 0 read.
//!
//! A baseline is recorded with `arthron gate --rebase`. Ruby's rate is Ruby's
//! own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod project;
pub mod resolve;

/// Ruby's registration. **Live**: the track owns `.rb`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for it and the
/// driver runs [`resolve::scan_ruby`] over every Ruby file the walk reaches.
pub const TRACK: Track = Track {
    name: "ruby",
    langs: &[Lang::Ruby],
    scan: Some(resolve::scan_ruby_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruby_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Ruby]);
        assert!(Lang::Ruby.owns_extension("rb"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("rb"));
        // Ruby reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::Ruby.domain(), crate::model::Domain::Ruby);
    }

    #[test]
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed `.rb` and deliberately left
        // `.gemspec`, `.ru` and `.rake` unclaimed; the honest moment to widen
        // that list is a commit that measures the files it adds, and this one
        // does not.
        assert_eq!(Lang::Ruby.extensions(), ["rb"]);
        for unclaimed in ["gemspec", "ru", "rake"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
    }
}
