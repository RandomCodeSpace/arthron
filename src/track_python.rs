//! The Python track. **Live.** Owns `.py`.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_python`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `py` and the
//! driver runs Python over every `.py` file the walk reaches. Three layers,
//! and the boundary between them is the project's first non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**. It is handed a
//!   path and a source string and has nothing it could link against.
//! - [`project`] — phase 0: which directories are packages, which are import
//!   roots, what the project declares as a dependency. Python states a
//!   module's name in packaging metadata rather than in the source, so this is
//!   what one `go.mod` line is to Go, spread across four formats.
//! - [`resolve`] — the one place a Python [`crate::Outcome`] is produced.
//!   Every reference ends `Resolved`, `External`, or `Unresolved(reason)`, and
//!   there is no way to express "dropped".
//!
//! [`stdlib`] holds the two name sets that come from outside any repository:
//! `sys.stdlib_module_names` and the builtins.
//!
//! Sharing the store with the other live tracks is safe in both directions: a
//! scan forgets only files carrying an extension the running track owns, and
//! extension ownership is a partition (see [`Lang::for_extension`]); the
//! manifest fence is per language, and Python's digest is empty when no
//! manifest was read, which is no opinion rather than a mismatch.
//!
//! # The honesty posture, and what it costs
//!
//! Python's unresolved floor is the largest of the four tier-1 languages and
//! is **supposed** to be. `x.m()` where `x` has no annotation genuinely needs
//! type inference; `self.client.get()` genuinely needs the type of an
//! attribute nobody declared. Both are
//! [`crate::UnresolvedReason::NeedsTypeInference`], and a first measurement
//! that is mostly that reason is the correct measurement.
//!
//! Two moves would raise the rate without linking anything, because both
//! [`crate::UnresolvedReason::LocalBinding`] and `External` sit outside *both*
//! terms of the rate:
//!
//! - **Widening `LocalBinding`.** Only a reference whose *whole* target is one
//!   block-bound name is a local binding. `c.send()` where `c` is a parameter
//!   names `send`, which is a node; it stays in the denominator and goes to
//!   the annotation table (E-05) and then to an honest reason. A function-local
//!   `import os` reports `locally_bound` for the very name it introduces
//!   (B-18) and still resolves as the module reference it is.
//! - **Widening `External`.** Go decides "standard library?" by asking whether
//!   the first path segment contains a dot. That test is *inverted* for Python
//!   — every third-party top-level name has no dot either — so [`stdlib`]
//!   embeds the frozen set instead, and a package that is neither standard
//!   library nor a declared dependency is
//!   [`crate::UnresolvedReason::UnknownPackage`] and counts against the rate.
//!
//! # Known under-counts
//!
//! Recorded here rather than left to be rediscovered, because each is a
//! *known* shortfall and none may be quietly closed by widening a bucket:
//!
//! - **Attribute reads.** `obj.x` that is not called is not a reference, so a
//!   `@property` read is a missing edge rather than a wrong one (E-10). A
//!   blanket read kind would multiply reference volume for modest gain.
//! - **Module-level `for`, `with` and `except` targets** bind module globals
//!   and are not emitted as definitions; only assignments, `def`, `class`,
//!   imports, `__slots__` and `global` writes are. References to such a name
//!   miss honestly rather than resolve to nothing quietly.
//! - **Framework string literals.** `mock.patch("pkg.mod.f")` (H-04) and
//!   `importlib.import_module("a.b")` (B-19) name things literally, and a
//!   framework rule — not the core extractor — is what turns them into
//!   references. Until such a rule exists both forms are ordinary calls: the
//!   call to `import_module` resolves as the standard-library call it is, and
//!   the specifier — literal or variable — is not a reference at all, so no
//!   edge is invented for a module named by a string. That means
//!   [`crate::UnresolvedReason::DynamicModuleSpecifier`] is currently
//!   unreachable in this track rather than a bucket the corpus fills; when the
//!   framework rule lands it is the reason the *variable* form must take, and
//!   a guessed target is never the alternative.
//! - **Cross-file supertypes.** Closed. The driver resolves this track's
//!   [`crate::lang::Resolver::link_kinds`] — `class C(B)` — before any member
//!   reference and leaves the relation in the store, so a base declared in
//!   another module is expanded transitively rather than probed once. What
//!   remains is what the relation itself says is short: a base that placed
//!   outside the repository, or at no definition at all, still leaves
//!   [`crate::UnresolvedReason::UnindexedSupertype`] below it. A chain that is
//!   *complete* and lacks the name is `NoMatchingDefinition`, which is the
//!   same answer an all-in-one-file hierarchy has always given.
//! - **Star-import chains.** `from x import *` re-exports the names `x`
//!   itself imported, transitively (B-10), so a source that star-imports in
//!   turn passes that chain on. The chain is followed one hop: the probes run
//!   against what the source *declares*, and a resolver holding one file's
//!   facts and a membership-only symbol table cannot see the second hop. A
//!   miss under any star import is therefore
//!   [`crate::UnresolvedReason::WildcardImport`] and not
//!   `NoMatchingDefinition` — the weaker claim is the true one. Closing it
//!   needs a module-facts pass of its own: the supertype phase settles what a
//!   *class* sits under, not what a module re-exports.
//! - **Re-export chains.** `from pkg import Foo` where `pkg/__init__.py`
//!   re-exports `Foo` resolves to the alias node in `pkg/__init__.py` — a real
//!   declaration site, one hop short of the definition, because the store does
//!   not surface [`crate::lang::Entry::Alias`].
//!
//! A baseline is recorded with `arthron gate --rebase`. Python's rate is
//! Python's own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod project;
pub mod resolve;
pub mod stdlib;

/// Python's registration. **Live**: the track owns `.py`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for it and the
/// driver runs [`resolve::scan_python`] over every Python file the walk
/// reaches.
pub const TRACK: Track = Track {
    name: "python",
    langs: &[Lang::Python],
    scan: Some(resolve::scan_python_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Python]);
        assert!(Lang::Python.owns_extension("py"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("py"));
        // Python reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::Python.domain(), crate::model::Domain::Python);
    }
}
