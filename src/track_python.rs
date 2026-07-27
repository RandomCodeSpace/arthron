//! The Python track. **Not live.** Owns `.py`.
//!
//! [`TRACK`] is registered with `scan: None`, so the driver runs nothing for
//! Python and [`crate::registry::Track::owns_extension`] answers `false` for
//! `py`. The extractor exists; the resolver that would produce an
//! [`crate::Outcome`] does not, and a track with no resolver has nothing to
//! contribute to a scan.
//!
//! # Going live
//!
//! Every step happens inside this file or under `src/track_python/`. Nothing
//! in `pipeline.rs`, `lib.rs`, `model.rs`, `registry.rs` or another track
//! moves.
//!
//! 1. **Submodules, nested.** *Done*: [`extract`] and [`lang`] resolve to
//!    `src/track_python/extract.rs` and `src/track_python/lang.rs`; a
//!    `mod resolve;` here will resolve to `src/track_python/resolve.rs`.
//! 2. **A [`crate::lang::Language`] impl.** *Done*: [`lang::PyLang`]. Its
//!    `Scope` and `Config` are `()` until the resolver needs them.
//!    `Config` is where the import root will live: a package is a directory,
//!    and which directory is the root is a project fact, so a layout it
//!    cannot determine is
//!    [`crate::UnresolvedReason::ProjectLayoutUnknown`] and not a guess.
//! 3. **An extractor** implementing [`crate::lang::Extractor`], parsing with
//!    [`crate::sg::SourceTree::parse_python`]. *Done*:
//!    [`extract::PyExtractor`]. It emits [`crate::model::Definition`] and
//!    [`crate::model::Reference`] records and **never an edge**.
//! 4. **A resolver** implementing [`crate::lang::Resolver`]: the one place a
//!    Python [`crate::Outcome`] is produced. Every reference ends `Resolved`,
//!    `External`, or `Unresolved(reason)`; nothing is dropped. A name bound
//!    by an assignment, parameter, comprehension, `with` or `except` clause
//!    ends `Unresolved(LocalBinding)` — reported beside `External` and
//!    excluded from both terms of the rate. The extractor has already stated
//!    that fact per reference in [`crate::model::Reference::locally_bound`];
//!    turning it into an outcome is this step's job, not the extractor's,
//!    because suppressing such a reference would delete it from the
//!    denominator instead of reporting it.
//! 5. **Honest reasons.** Python's floor is the largest of the four and is
//!    supposed to be: `x.m()` where `x` has no annotation is
//!    [`crate::UnresolvedReason::NeedsTypeInference`], a monkeypatch is
//!    [`crate::UnresolvedReason::Generated`] or a `Rebind` reference, a
//!    `from m import *` whose source cannot be enumerated is
//!    [`crate::UnresolvedReason::WildcardImport`]. A first measurement that
//!    is mostly `NeedsTypeInference` is the correct measurement. Moving any
//!    of it into `LocalBinding` or `External` takes it out of the rate's
//!    denominator and raises the number without linking anything; that is the
//!    failure mode this track is reviewed for.
//! 6. **An entry point** with the shape of [`crate::registry::TrackScan`]:
//!    `fn scan_python(root, db)`, whose body is
//!    `crate::pipeline::scan::<PyLang>(root, db, &PyExtractor, &PyResolver)`.
//! 7. **Flip the switch here**: `scan: None` becomes `scan: Some(scan_python)`.
//! 8. **A baseline**, recorded with `arthron gate --rebase`. Python's rate is
//!    Python's own and is never averaged into anyone else's.
//!
//! Sharing the store with a live Go track is safe: a scan forgets only files
//! carrying an extension the running track owns, and extension ownership is a
//! partition (see [`Lang::for_extension`]).
//!
//! # What the extractor does not see yet
//!
//! Recorded here rather than left to be rediscovered, because each is a
//! *known* under-count and none of them may be quietly closed by widening a
//! bucket:
//!
//! - **Attribute reads.** `obj.x` that is not called is not a reference, so a
//!   `@property` read is a missing edge rather than a wrong one (E-10). A
//!   blanket read kind would multiply reference volume for modest gain.
//! - **Module-level `for`, `with` and `except` targets** bind module globals
//!   and are not emitted as definitions; only assignments, `def`, `class`,
//!   imports, `__slots__` and `global` writes are. References to such a name
//!   will miss honestly rather than resolve to nothing quietly.
//! - **Framework string literals.** `mock.patch("pkg.mod.f")` (H-04) and
//!   `importlib.import_module("a.b")` (B-19) name things literally, and a
//!   framework rule — not the core extractor — is what turns them into
//!   references. Until then they are ordinary calls, and the *variable*
//!   forms must stay [`crate::UnresolvedReason::DynamicModuleSpecifier`],
//!   never a guess.
//! - **The annotation-to-name map.** Annotations are emitted as
//!   [`crate::model::RefKind::TypeUse`] references, which is what makes
//!   E-05's `def f(c: Client): c.send()` resolvable without inference, but
//!   the per-block `name → annotated type` table those feed is the
//!   resolver's scope and lands with it.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod project;
pub mod resolve;
pub mod stdlib;

/// Python's registration. `scan: None`: the track owns no file and
/// contributes nothing to a scan until the work above lands.
pub const TRACK: Track = Track {
    name: "python",
    langs: &[Lang::Python],
    scan: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_is_registered_but_not_live() {
        assert!(!TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Python]);
        assert!(Lang::Python.owns_extension("py"));
        assert!(!TRACK.owns_extension("py"));
    }
}
