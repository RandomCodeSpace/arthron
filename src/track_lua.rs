//! The Lua track. **Live.** Owns `.lua`, at **tier 2**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_lua_with`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `lua` and
//! the driver runs Lua over every `.lua` file the walk reaches. Four layers,
//! and the boundary between them is the project's first non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`project`] — phase 0: the rockspec's `build.modules` table, which is
//!   the only place in a Lua repository that maps a module name to a file.
//! - [`resolve`] — the one place a Lua [`crate::Outcome`] is produced. Every
//!   reference ends `Resolved`, `External`, or `Unresolved(reason)`, and
//!   there is no way to express "dropped".
//! - [`lang`] — the [`crate::lang::Language`] impl and the FQN grammar the
//!   other three agree on.
//!
//! # What tier 2 means here, precisely
//!
//! Definitions, structure, and imports. **No call edges and no type-use
//! resolution**, and the honest consequence is that the extractor emits no
//! call reference *at all*: a tier-2 language that emitted them un-gated
//! would put references into a denominator nothing in this track resolves,
//! and report tier-1 coverage it has not measured.
//!
//! Lua makes that line harder to hold than any other language on this list,
//! because **`require` is itself an ordinary function call**. There is no
//! import statement, no keyword, and nothing in the grammar that separates
//! loading a module from calling a function — only the callee's name does.
//! So this extractor reads exactly two call shapes, `require <specifier>` and
//! `pcall(require, <specifier>)`, and every other call in the tree
//! contributes nothing.
//!
//! Lua's gate is therefore an **import-resolution rate**, and it is not
//! comparable with Go's or Java's, nor with Ruby's or Scala's.
//!
//! # The two numbers a reader of the baseline should expect
//!
//! - **`local_binding` is zero, and stays zero.** It is one of the two
//!   buckets the rate's own definition lets a resolver move references into
//!   without linking anything. Tier 2 emits no expression-level reference, so
//!   nothing here *can* name a local; a non-zero count would mean the
//!   contract above had been widened, and the baseline fails on drift in it.
//! - **`external` is zero, and stays zero.** It is the other one. A rockspec
//!   declares *rock* names and a rock name is not a module name — the
//!   measured corpus declares nine and refutes the identification six times
//!   (`penlight` ships `pl.*`, `lua-term` ships `term`, `lua_cliargs` ships
//!   `cliargs`, `mediator_lua` ships `mediator`, `luasystem` ships `system`,
//!   `lua` ships the standard library). Every path that leaves the repository
//!   is [`crate::UnresolvedReason::ModuleNotFound`] and counts *against* the
//!   rate. See [`resolve`] for the full argument, including why the miss is
//!   not `UnknownPackage`.
//!
//! Both are the deliberately expensive answers, and together they leave this
//! rate with no bucket a future change could quietly move a reference into.
//!
//! Sharing the store with the other live tracks is safe in both directions: a
//! scan forgets only files carrying an extension the running track owns, and
//! extension ownership is a partition (see
//! [`crate::model::Lang::for_extension`]); the manifest fence is per
//! language, and Lua's digest covers exactly what phase 0 read.
//!
//! A baseline is recorded with `arthron gate --rebase`. Lua's rate is Lua's
//! own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod project;
pub mod resolve;

/// Lua's registration. **Live**: the track owns `.lua`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for it and the
/// driver runs [`resolve::scan_lua`] over every Lua file the walk reaches.
pub const TRACK: Track = Track {
    name: "lua",
    langs: &[Lang::Lua],
    scan: Some(resolve::scan_lua_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Lua]);
        assert!(Lang::Lua.owns_extension("lua"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("lua"));
        // Lua reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::Lua.domain(), crate::model::Domain::Lua);
    }

    #[test]
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed `.lua` and deliberately left
        // everything else unclaimed; the honest moment to widen that list is
        // a commit that measures the files it adds, and this one does not.
        // `.rockspec` in particular is read by `project` as a *manifest* —
        // the same way Ruby reads a gemspec — and a manifest is not a file
        // whose definitions belong in the graph.
        assert_eq!(Lang::Lua.extensions(), ["lua"]);
        for unclaimed in ["rockspec", "luacheckrc", "moon", "tl", "luacov"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
    }
}
