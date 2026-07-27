//! The C++ track. **Live.** Owns `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh` and
//! `.hxx`, at **tier 2**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_cpp`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for those six
//! and the driver runs C++ over every file the walk reaches under one. Four
//! layers, and the boundary between them is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`project`] — phase 0: where an `#include` starts looking, and what this
//!   repository publishes there that this build does not parse. C++ states
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
//! # The one number a reader of the baseline must not misread
//!
//! **fmt's headers are all `.h`, and `.h` is an extension this build does not
//! claim.** The tier-2 registration left `.c` and `.h` unclaimed — a C
//! translation unit read under the C++ grammar is the wrong language — and
//! going live widened nothing. The corpus is a header-dominated library: 21
//! of its 55 source files are `.h`, and 99 of the 116 quoted `#include`
//! directives in the 33 files this track *does* read name one of them.
//!
//! So those 99 references, and one angled `<fmt/base.h>` beside them, are
//! `Unresolved` with a reason: a literal specifier that resolved to no module
//! under this build's configured resolution, whose translation units are the
//! six extensions above. They are **not** `External` — laundering an
//! in-repository header into the bucket that sits outside the rate is exactly
//! the failure the Rust review caught one language earlier — so they count
//! *against* the rate, and the rate is correspondingly small.
//!
//! That is a floor of the extension policy, not a resolver gap, and it is
//! stated here so nobody reads the number as a broken track. The rate is
//! still a ratchet: it can only be re-based upward, and the single change
//! that would move it most is a *separate, ratified* decision to claim `.h`,
//! which is a claim about parsing C headers under a C++ grammar and not
//! something a go-live commit may make on its own.
//!
//! # Known limits, recorded rather than left to be rediscovered
//!
//! - **The preprocessor is not evaluated.** Every `#include` in a file is a
//!   reference, including the ones inside a `#if` no scan can decide.
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
        for ext in ["cpp", "cc", "cxx", "hpp", "hh", "hxx"] {
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
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed six extensions and deliberately
        // left `.c` and `.h` unclaimed. Claiming either is a claim about
        // reading a C translation unit under the C++ grammar, and it is a
        // decision of its own — not something this commit makes on the way
        // past, however much it would move the measured rate.
        assert_eq!(
            Lang::Cpp.extensions(),
            ["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
        );
        for unclaimed in ["c", "h"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
    }
}
