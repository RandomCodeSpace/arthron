//! The C# track. **Live.** Owns `.cs`, at **tier 2**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_csharp`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `cs` and the
//! driver runs C# over every `.cs` file the walk reaches. Going live edited
//! this file and this track's own modules, which is the whole of the
//! registry's zero-conflict rule — see [`crate::registry`] for why, and
//! [`crate::track_go`] for the shape a live track takes.
//!
//! # Tier 2 is a smaller promise, kept exactly
//!
//! Tier 1 is definitions, references, and cross-file import and
//! function-call resolution — the reference kinds in its denominator, not a
//! complete call graph. **Tier 2 is definitions, structure and imports, and
//! its gate is an import-resolution rate.** So this track emits one reference
//! kind — [`crate::model::RefKind::Import`], the `using` directive — and no
//! call site, no type use and no supertype. That is a deliberate refusal
//! rather than an unfinished job: a tier-2 language that emitted them
//! un-gated would put references in a denominator no tier-2 resolver links,
//! which is tier-1 coverage claimed without tier-1 work.
//!
//! What tier 2 keeps in full is the never-drop contract. Every `using` this
//! track extracts ends `Resolved`, `External`, or `Unresolved` with a reason
//! from the ratified taxonomy — none was added for C#.
//!
//! Three layers, and the boundary between them is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**. It is also
//!   where C#'s two genuinely hostile pieces of syntax are settled: `using`
//!   means three unrelated things depending on where it sits, and `#if` is
//!   read rather than evaluated.
//! - [`lang`] — the constants, and the FQN grammar the other two agree on.
//! - [`resolve`] — the one place a C# [`crate::Outcome`] is produced.
//!
//! # There is no phase 0, and that is a decision
//!
//! Every other tier-2 track has one: Ruby's load roots, PHP's PSR-4 map,
//! Rust's workspace. C# has none, because no manifest mediates between a name
//! and where it lives — a type's namespace is stated in its own source, and a
//! `using` names an absolute name. What a `.csproj` decides is which
//! *assemblies* a compilation sees and which `FEATURE_*` symbols are defined,
//! and this track resolves neither: assembly visibility cannot change an
//! answer on a corpus that compiles (a `using` the project cannot see does not
//! build), and both arms of every `#if` are read. So
//! [`resolve::CsProject`] is empty and its digest is too — the case
//! [`crate::lang::Resolver::config_digest`] names as "a language with no
//! project manifest".
//!
//! # What the numbers look like, and why
//!
//! - **The denominator is small on purpose.** C# 10's `global using` and the
//!   SDK's implicit usings mean a file usually imports nothing: 169 of the
//!   corpus's 193 files carry no `using` at all, and 65 of its 89 directives
//!   sit in three `GlobalUsings.cs` files. That is what C# looks like, not
//!   what this extractor missed — and the definition census beside the rate
//!   is what checks the other half of tier 2's deliverable.
//! - **`external` is the load-bearing number.** A namespace this repository
//!   does not declare is declared by another assembly, so the corpus's 33
//!   `System.*` directives, its two `Xunit` ones and its one `Newtonsoft.Json`
//!   are [`crate::Outcome::External`] and sit outside both terms of the rate. [`resolve`] says at length why that is a
//!   measured fact here and not the cheap way to raise a rate; the gate
//!   fails on any drift in the count.
//! - **`local_binding` is zero, and stays zero.** Tier 2 emits no
//!   expression-level reference, so nothing here *can* name a local. A
//!   non-zero count would mean the contract above had been widened.
//!
//! # Known limits, recorded rather than left to be rediscovered
//!
//! - **A `#if` that splits a declaration in half breaks the parse.** Two of
//!   the corpus's 193 files put `#if`/`#else`/`#endif` between a method's
//!   signature and its body; tree-sitter-c-sharp cannot represent that and
//!   recovers with `ERROR` nodes that swallow the enclosing type. Those files
//!   contribute their namespace and their imports and no type or member.
//!   See [`extract`].
//! - **A nested type under a `using static` miss** is classified `External`
//!   rather than [`crate::UnresolvedReason::NoMatchingDefinition`]. See
//!   [`resolve`].
//! - **`InternalsVisibleTo` is not a fact this graph holds.** Recovering it
//!   means reading a target out of an attribute's string literal, which the
//!   framework-fact layer is for and the core extractor is not.
//!
//! Sharing the store with the other live tracks is safe in both directions: a
//! scan forgets only files carrying an extension the running track owns, and
//! extension ownership is a partition (see
//! [`crate::model::Lang::for_extension`]); the manifest fence is per language,
//! and C# has no manifest to fence on. C# takes
//! [`crate::model::Domain::CSharp`] alone — no other language's resolver has
//! to find a C# declaration, and a shared identity space is a capability claim
//! rather than a convenience.
//!
//! A baseline is recorded with `arthron gate --rebase`. C#'s rate is C#'s own
//! and is never averaged into anyone else's — and it is an import rate, which
//! is not the same measurement a tier-1 language's rate is.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod resolve;

/// C#'s registration. **Live**: the track owns `.cs`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for it and the
/// driver runs [`resolve::scan_csharp`] over every C# file the walk reaches.
pub const TRACK: Track = Track {
    name: "csharp",
    langs: &[Lang::CSharp],
    scan: Some(resolve::scan_csharp_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csharp_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::CSharp]);
        assert!(Lang::CSharp.owns_extension("cs"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("cs"));
        // C# reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::CSharp.domain(), crate::model::Domain::CSharp);
    }

    #[test]
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed `.cs` and deliberately left
        // `.csx`, `.razor` and `.cshtml` unclaimed; the honest moment to
        // widen that list is a commit that measures the files it adds, and
        // this one does not.
        assert_eq!(Lang::CSharp.extensions(), ["cs"]);
        for unclaimed in ["csx", "razor", "cshtml"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
    }
}
