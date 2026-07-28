//! The Dart track. **Live.** Owns `.dart`, at **tier 2, best effort**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_dart_with`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `dart` and
//! the driver runs Dart over every `.dart` file the walk reaches. Four layers,
//! and the boundary between the first three is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`project`] — phase 0: what `pubspec.yaml` calls this package, and which
//!   packages it declares. Dart states neither in its source.
//! - [`resolve`] — the one place a Dart [`crate::Outcome`] is produced. Every
//!   reference ends `Resolved`, `External`, or `Unresolved(reason)`, and there
//!   is no way to express "dropped".
//! - [`lang`] — the [`crate::lang::Language`] impl and the FQN grammar the
//!   other three agree on.
//!
//! # What tier 2, best effort means here, stated so nobody has to infer it
//!
//! Definitions, structure, and the URIs the library directives name.
//! **No call edges and no type-use resolution**, and the honest consequence is
//! that the extractor emits no call and no type reference *at all*: a tier-2
//! language that emitted them un-gated would put references into a denominator
//! nothing in this track resolves, and report tier-1 coverage it has not
//! measured. `class C extends B` is therefore read as part of `C`'s structure
//! and produces no [`crate::model::RefKind::Inherit`] reference.
//!
//! **Best effort** is one further line, drawn at the same place: a `show`/
//! `hide` combinator names *declarations inside another library*, and pricing
//! one means computing that library's exported name set through every barrel
//! it re-exports. This track does not, so it emits no reference for a
//! combinator and records the names as structure instead. A name honestly not
//! counted is worth more than a name in a denominator answered by guessing.
//!
//! So Dart's gate is an **import-resolution rate**, and it is not comparable
//! with Go's or Java's, or with Ruby's. One reference per **URI**, not per
//! directive: a configurable import names two libraries and both are resolved.
//!
//! # What a reader of the baseline should expect
//!
//! - **`local_binding` is zero, and stays zero.** It is the one bucket the
//!   rate's own definition lets a resolver move references into without
//!   linking anything. Tier 2 emits no expression-level reference, so nothing
//!   here *can* name a local; a non-zero count would mean the contract above
//!   had been widened, and the baseline fails on drift in it.
//! - **`external` is large, and every one of it is spelled by a scheme or by
//!   the manifest.** A `dart:` URI is external because the language reserves
//!   that scheme for the SDK — no repository file can be addressed by one — and
//!   a `package:<name>` URI is external only when `pubspec.yaml` declares
//!   `<name>` and does *not* place it inside this tree with a `path:`. This
//!   repository's own package name is tested first, and a `path:` dependency
//!   is a lookup under that package's own `lib/`, so a `package:` URI naming
//!   anything this repository contains is a lookup that can miss and never an
//!   `External` that cannot. See [`resolve`] for why that ordering is the
//!   whole defence against laundering a rate.
//!
//! A baseline is recorded with `arthron gate --rebase`. Dart's rate is Dart's
//! own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod project;
pub mod resolve;

/// Dart's registration. **Live**: the track owns `.dart`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for it and the
/// driver runs [`resolve::scan_dart`] over every Dart file the walk reaches.
pub const TRACK: Track = Track {
    name: "dart",
    langs: &[Lang::Dart],
    scan: Some(resolve::scan_dart_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dart_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Dart]);
        assert!(Lang::Dart.owns_extension("dart"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("dart"));
        // Dart reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::Dart.domain(), crate::model::Domain::Dart);
    }

    #[test]
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed `.dart` and nothing else; the
        // honest moment to widen that list is a commit that measures the files
        // it adds, and this one does not. `.g.dart` and `.freezed.dart` are
        // generated Dart and are already `.dart` — they are a walk question,
        // not an extension one.
        assert_eq!(Lang::Dart.extensions(), ["dart"]);
        for unclaimed in ["yaml", "pubspec", "kt"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
    }
}
