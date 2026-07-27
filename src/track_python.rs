//! The Python track. **Not live.** Owns `.py`.
//!
//! [`TRACK`] is registered with `scan: None`, so the driver runs nothing for
//! Python and [`crate::registry::Track::owns_extension`] answers `false` for
//! `py`. The seam exists; the work does not.
//!
//! # Going live
//!
//! Every step happens inside this file or under `src/track_python/`. Nothing
//! in `pipeline.rs`, `lib.rs`, `model.rs`, `registry.rs` or another track
//! moves.
//!
//! 1. **Submodules, nested.** `mod extract;`, `mod resolve;` here resolve to
//!    `src/track_python/extract.rs` and `src/track_python/resolve.rs`;
//!    `lib.rs` already declares `track_python`.
//! 2. **A [`crate::lang::Language`] impl**, say `PyLang`, with
//!    `const LANG = Lang::Python`, `const DOMAIN = Domain::Python`,
//!    `extensions()` returning [`Lang::extensions`] for `Lang::Python` rather
//!    than a second list, `skip_dirs()` for virtual environments and caches
//!    (`.venv`, `venv`, `__pycache__`, `.tox`), and the three associated
//!    types. `Config` is where the import root lives: a package is a
//!    directory, and which directory is the root is a project fact, so a
//!    layout it cannot determine is
//!    [`crate::UnresolvedReason::ProjectLayoutUnknown`] and not a guess.
//! 3. **An extractor** implementing [`crate::lang::Extractor`], parsing with
//!    [`crate::sg::SourceTree::parse_python`]. It emits
//!    [`crate::model::Definition`] and [`crate::model::Reference`] records and
//!    **never an edge**.
//! 4. **A resolver** implementing [`crate::lang::Resolver`]: the one place a
//!    Python [`crate::Outcome`] is produced. Every reference ends `Resolved`,
//!    `External`, or `Unresolved(reason)`; nothing is dropped. A name bound
//!    by an assignment, parameter, comprehension, `with` or `except` clause
//!    ends `Unresolved(LocalBinding)` — reported beside `External` and
//!    excluded from both terms of the rate.
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

use crate::model::Lang;
use crate::registry::Track;

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
