//! The Elixir track. **Live.** Owns `.ex` and `.exs`, at **tier 2**,
//! best-effort.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_elixir_with`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for both
//! extensions and the driver runs Elixir over every file the walk reaches.
//! Three layers, and the boundary between them is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`resolve`] — the one place an Elixir [`crate::Outcome`] is produced.
//!   Every reference ends `Resolved`, `External`, or `Unresolved(reason)`,
//!   and there is no way to express "dropped".
//! - [`lang`] — the [`crate::lang::Language`] impl, the FQN grammar the other
//!   two agree on, and [`lang::ElixirProject`], which is empty on purpose.
//!
//! **There is no `project` module, and that is a measured fact rather than an
//! omission.** Not one reference in the vendored corpus names a file, a path
//! or a directory — Elixir has no import-by-path form at all. A directive
//! names a module by the atom it compiles to, a `defmodule` decides that
//! atom, and both are in the tree the walk already read. `mix.exs` is read as
//! source like any other `.exs` file and never as configuration; see
//! [`lang::ElixirProject`] for why its `deps` list would buy a guess.
//!
//! # What tier 2, best-effort means here, stated so nobody has to infer it
//!
//! Definitions, structure, and import-like references. **No call edges, no
//! type-use resolution, and no expression-level reference**, and the honest
//! consequence is that the extractor emits none of them *at all*: a tier-2
//! language that emitted them un-gated would put references into a
//! denominator nothing in this track resolves, and report tier-1 coverage it
//! has not measured. `@behaviour Plug` is therefore read as part of a
//! module's structure and produces no [`crate::model::RefKind::Inherit`]
//! reference, and `defdelegate f(x), to: M` produces no call.
//!
//! So Elixir's gate is an **import-resolution rate** over the four
//! directives — `alias`, `import`, `require`, `use` — one reference per
//! module named, and it is not comparable with Go's or Java's or another
//! tier-2 language's.
//!
//! # The two numbers a reader of the baseline should expect
//!
//! - **`local_binding` is zero, and stays zero.** It is the one bucket the
//!   rate's own definition lets a resolver move references into without
//!   linking anything. Tier 2 emits no expression-level reference, so nothing
//!   here *can* name a local; a non-zero count would mean the contract above
//!   had been widened, and the baseline fails on drift in it.
//! - **`external` is large, and every entry is pinned by name.** Elixir's
//!   standard library, OTP and every hex dependency live outside this
//!   repository, and a module name is the exact identity of each — so unlike
//!   Ruby, this track can name what it did not index without guessing. That
//!   is what makes `External` the right answer here and the wrong one there;
//!   [`resolve`] carries the argument, and `tests/elixir_corpus.rs` pins the
//!   set, because `External` sits outside both rate terms and an
//!   in-repository module filed into it would vanish from the measurement
//!   rather than fail it.
//!
//! A baseline is recorded with `arthron gate --rebase`. Elixir's rate is
//! Elixir's own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod resolve;

/// Elixir's registration. **Live**: the track owns `.ex` and `.exs`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for both and the
/// driver runs [`resolve::scan_elixir`] over every Elixir file the walk
/// reaches.
pub const TRACK: Track = Track {
    name: "elixir",
    langs: &[Lang::Elixir],
    scan: Some(resolve::scan_elixir_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elixir_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Elixir]);
        assert!(Lang::Elixir.owns_extension("ex"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("ex"));
        assert!(TRACK.owns_extension("exs"));
        // Elixir reports one rate, under its own language code, and shares an
        // identity space with nobody — Erlang included, which this build does
        // not read at all.
        assert_eq!(Lang::Elixir.domain(), crate::model::Domain::Elixir);
    }

    #[test]
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed `.ex` and `.exs` and deliberately
        // left the template dialects unclaimed; the honest moment to widen
        // that list is a commit that measures the files it adds, and this one
        // does not.
        assert_eq!(Lang::Elixir.extensions(), ["ex", "exs"]);
        for unclaimed in ["eex", "heex", "leex", "erl", "hrl"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
    }

    // Both halves of the extension claim are measured against the corpus in
    // `tests/elixir_corpus.rs`, where the walk's own file set is split by
    // extension. A unit test here could only re-assert the static list the
    // two tests above already pin, and would stay green if the corpus lost
    // every `.exs` file.
}
