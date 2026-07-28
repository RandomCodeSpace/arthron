//! Lua's [`Language`] impl: the constants the track is reported under, the
//! three types only Lua's own layers may read, and the FQN grammar every one
//! of them agrees on.
//!
//! # The FQN grammar
//!
//! Lua names two different kinds of thing, and one identity space has to hold
//! both without either being able to spell the other:
//!
//! - A **chunk** — what `require` names. One per `.lua` file the walk
//!   reaches, written `$` + the repo-relative path with `.lua` stripped:
//!   `$busted/core`, `$spec/strict`.
//! - A **member** of a chunk, written `$chunk#path.as.written`:
//!   `$busted/block#block.reject`, `$busted/compatibility#exit`.
//!
//! Two reserved characters, both already ratified by the core. `$` opens the
//! chunk space and can appear in no Lua name — it is not a character a Lua
//! identifier may carry at all — so no member FQN can collide with a chunk
//! FQN whatever a repository names its directories. `#` separates a container
//! from its members exactly as it does in every other domain here, so a chunk
//! FQN carries none and a member FQN carries exactly one. `:` appears in
//! neither, which is what [`crate::pipeline`]'s `external:` prefix rests on.
//!
//! # Why a member is named under its chunk and never globally
//!
//! `local M = {}` binds a **local**, and `function M.foo()` writes a key into
//! the table that local holds. Nothing about either name is visible to
//! another file except through the value the chunk returns, so two files that
//! both write `function M.foo()` have written two different functions. Naming
//! a member under its chunk is what keeps them two nodes.
//!
//! A bare `function f()` really does write `_G.f`, and by the same rule it is
//! still filed under its chunk. That is a deliberate under-claim: this track
//! tracks no `_G`, emits no reference that could name one, and inventing a
//! global identity for a name whose binding depends on whether some enclosing
//! block wrote `local f` first would be a guess. It is recorded in
//! [`crate::track_lua::extract`] among the under-counts.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_lua::extract::LuaHeader;
use crate::track_lua::project::LuaProject;
use crate::track_lua::resolve::LuaScope;

/// The Lua language. Stateless; only its associated types carry anything.
pub struct LuaLang;

impl Language for LuaLang {
    const LANG: Lang = Lang::Lua;
    const DOMAIN: Domain = Domain::Lua;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Lua owns and this one cannot drift apart.
    ///
    /// `.rockspec` and `.luacheckrc` are Lua source and are deliberately
    /// **not** claimed. A rockspec is read by [`crate::track_lua::project`]
    /// as a manifest — the same way Ruby reads a gemspec — and a manifest is
    /// not a file whose definitions belong in the graph.
    fn extensions() -> &'static [&'static str] {
        Lang::Lua.extensions()
    }

    /// Directories holding installed rocks. Descending into one would index a
    /// dependency as if the repository had written it, inventing
    /// in-repository definitions that inflate the resolution rate.
    fn skip_dirs() -> &'static [&'static str] {
        &["lua_modules", ".luarocks"]
    }

    type Header = LuaHeader;
    type Scope = LuaScope;
    type Config = LuaProject;
}

/// The reserved prefix a chunk identity carries, and nothing else may.
pub const CHUNK: char = '$';

/// The chunk FQN of a repo-relative path: `busted/core.lua` → `$busted/core`.
///
/// Total, because every `.lua` file the walk reaches is a chunk whether or
/// not it declares anything, and a `require` naming an empty file still
/// resolves.
pub fn chunk_fqn(rel_path: &str) -> String {
    format!(
        "{CHUNK}{}",
        rel_path.strip_suffix(".lua").unwrap_or(rel_path)
    )
}

/// The FQN of a member of a chunk: `("$busted/block", ["block", "reject"])` →
/// `$busted/block#block.reject`.
///
/// `None` for an empty path: "this file does not say" is not the same as
/// naming the empty string.
pub fn member_fqn(chunk: &str, path: &[String]) -> Option<String> {
    if path.is_empty() || path.iter().any(String::is_empty) {
        return None;
    }
    Some(format!("{chunk}#{}", path.join(".")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_reports_as_lua_and_hashes_in_the_lua_domain() {
        assert_eq!(LuaLang::LANG, Lang::Lua);
        assert_eq!(LuaLang::DOMAIN, Domain::Lua);
        assert_eq!(LuaLang::LANG.domain(), LuaLang::DOMAIN);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(LuaLang::extensions(), Lang::Lua.extensions());
        assert_eq!(LuaLang::extensions(), ["lua"]);
        for unclaimed in ["rockspec", "luacheckrc", "moon", "tl"] {
            assert!(!LuaLang::extensions().contains(&unclaimed));
        }
    }

    #[test]
    fn a_chunk_identity_cannot_be_spelled_by_a_member() {
        assert_eq!(chunk_fqn("busted/core.lua"), "$busted/core");
        assert_eq!(chunk_fqn("spec/strict.lua"), "$spec/strict");
        // No `.lua` is still a chunk: the walk only offers `.lua`, and a name
        // that lost its suffix must not silently become another file's.
        assert_eq!(chunk_fqn("bin/busted"), "$bin/busted");
        assert!(chunk_fqn("busted.lua").starts_with(CHUNK));
    }

    #[test]
    fn a_member_carries_exactly_one_reserved_separator() {
        let chunk = chunk_fqn("busted/block.lua");
        let member = member_fqn(&chunk, &["block".into(), "reject".into()]).expect("nameable");
        assert_eq!(member, "$busted/block#block.reject");
        assert_eq!(member.matches('#').count(), 1);
        // `:` is reserved for the `external:` prefix and appears in no FQN
        // this domain mints — `function M:bar()` is normalised to `M.bar`,
        // which is the key Lua itself writes.
        assert!(!member.contains(':'));
        // A chunk that names its exports in a returned table literal writes
        // them straight under the chunk: there is no table name to carry.
        assert_eq!(
            member_fqn(&chunk_fqn("busted/compatibility.lua"), &["exit".into()]).as_deref(),
            Some("$busted/compatibility#exit"),
        );
    }

    #[test]
    fn a_path_that_says_nothing_names_nothing() {
        let chunk = chunk_fqn("a.lua");
        assert_eq!(member_fqn(&chunk, &[]), None);
        assert_eq!(member_fqn(&chunk, &["".into()]), None);
        assert_eq!(member_fqn(&chunk, &["M".into(), "".into()]), None);
    }
}
