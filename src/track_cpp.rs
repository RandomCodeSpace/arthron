//! The C++ track. **Live.** Owns `.cpp`, `.cc`, `.cxx`, `.h`, `.hpp`,
//! `.hh` and `.hxx`, at **tier 2**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_cpp`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for those seven
//! and the driver runs C++ over every file the walk reaches under one. Four
//! layers, and the boundary between them is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`project`] — phase 0: where an `#include` starts looking, and what this
//!   repository publishes there that this scan holds no node for. C++ states
//!   neither in its source, and unlike every language already here it states
//!   them in no manifest either.
//! - [`lang`] — the [`crate::lang::Language`] impl and the FQN grammar the
//!   other three agree on: three identity spaces, two of them behind a
//!   reserved character.
//! - [`resolve`] — the one place a C++ [`crate::Outcome`] is produced. Every
//!   reference ends `Resolved`, `External`, or `Unresolved(reason)`, and
//!   there is no way to express "dropped".
//!
//! # What tier 2 means here, precisely
//!
//! Definitions, structure, and imports. **No call edges and no type-use
//! resolution**, and the honest consequence is that the extractor emits no
//! call or type reference *at all*. C++'s gate is an **import-resolution
//! rate**: the reference kinds this track emits are `#include` — quoted,
//! angled, or macro-spelled — and the C++20 `import`, and the definitions
//! beside them are the structure: namespaces, classes, structs, unions,
//! enumerations and their constants, functions and member functions, type
//! aliases, the module a file exports, and the *unit* every file is.
//!
//! # `.h` is claimed, and the claim is the amendment the registration reserved
//!
//! The tier-2 registration left `.c` and `.h` unclaimed — a C translation
//! unit read under the C++ grammar is the wrong language — and this track
//! went live owning six extensions, which on the measured corpus cost the
//! measurement itself. fmt is a header-dominated library whose headers are
//! all `.h`: 21 of its 55 source files were invisible, 100 in-repository
//! header references were `Unresolved` as an extension-policy floor, and
//! the rate came out at 3.4% — a number that measured the policy, not the
//! resolver. The registration reserved the widening for "the commit that
//! parses it", and this is that commit: `.h` rides the C++ track, the scan
//! reads the headers, and the re-based rate measures resolution.
//!
//! The accepted risk, stated rather than hidden: a pure-C repository's
//! `.h` files now parse under the C++ grammar. That stands until a C track
//! exists and arbitrates ownership — `.c` stays unclaimed, because no
//! commit yet parses a C translation unit as C.
//!
//! # Known limits, recorded rather than left to be rediscovered
//!
//! - **The preprocessor is not evaluated.** Every `#include` in a file is a
//!   reference, including the ones inside a `#if` no scan can decide. Holding
//!   that costs one length-preserving pass over the bytes, because a `#if`
//!   condition the pinned grammar cannot parse otherwise swallows every
//!   directive after it. See [`extract`].
//! - **The pinned grammar has no C++20 modules.** `export module fmt;` comes
//!   back with an `ERROR` node in it and `import fmt;` is shaped like a
//!   variable declaration, so both are read off the token sequence the
//!   misparse leaves — narrowly, and by fixture. See [`extract`].
//! - **Include roots come from the tree layout, not from `CMakeLists.txt`.**
//!   There is no CMake grammar in this build, and a regular expression over a
//!   Turing-complete build language is the thing the Ruby track refused. See
//!   [`project`] for why the layout says the same thing and how the corpus's
//!   own build files corroborate it.
//! - **Macros are not definitions, and neither are data members.** Both are
//!   argued where they bite, in [`extract`].
//!
//! Sharing the store with the other live tracks is safe in both directions: a
//! scan forgets only files carrying an extension the running track owns, and
//! extension ownership is a partition (see
//! [`crate::model::Lang::for_extension`]); the manifest fence is per language,
//! and C++'s digest covers exactly what phase 0 read.
//!
//! A baseline is recorded with `arthron gate --rebase`. C++'s rate is C++'s
//! own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod project;
pub mod resolve;

/// C++'s registration. **Live**: the track owns the six extensions
/// [`Lang::Cpp`] claims, so [`crate::registry::Track::owns_extension`] answers
/// `true` for them and the driver runs [`resolve::scan_cpp`] over every C++
/// file the walk reaches.
pub const TRACK: Track = Track {
    name: "cpp",
    langs: &[Lang::Cpp],
    scan: Some(resolve::scan_cpp_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpp_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Cpp]);
        for ext in ["cpp", "cc", "cxx", "h", "hpp", "hh", "hxx"] {
            assert!(Lang::Cpp.owns_extension(ext));
            // Extension ownership is a property of the language whether or
            // not anything is built for it; whether a scan reads such a file
            // is a property of the track, and the track now says yes.
            assert!(TRACK.owns_extension(ext));
        }
        // C++ reports one rate, under its own language code, and shares an
        // identity space with nobody — `Domain::Cxx` is named for the family
        // so C could join it, and no C support has landed.
        assert_eq!(Lang::Cpp.domain(), crate::model::Domain::Cxx);
    }

    #[test]
    fn the_h_claim_is_the_one_ratified_widening_and_c_stays_unclaimed() {
        // The tier-2 registration committed six extensions and reserved
        // `.h` for a decision of its own — the first honest moment to
        // claim an extension is the commit that parses it. This build
        // parses `.h`, so the list is exactly one wider. `.c` is still a
        // claim about reading a C translation unit under the C++ grammar,
        // and nobody has ratified it.
        assert_eq!(
            Lang::Cpp.extensions(),
            ["cpp", "cc", "cxx", "h", "hpp", "hh", "hxx"],
        );
        assert!(TRACK.owns_extension("h"));
        assert!(!TRACK.owns_extension("c"));
    }
}
