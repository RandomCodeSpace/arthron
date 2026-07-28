//! The one place a Lua [`crate::Outcome`] is produced. Never drops.
//!
//! # The import model, as measured
//!
//! Lua has no import statement. `require` is an ordinary function that takes
//! a string, and the string is turned into a file by searching
//! `package.path` — a mutable global whose value at the moment of the call is
//! not a property of the source tree. Three rules follow, and each is a rule
//! about *where a name is looked up* rather than about what it might mean:
//!
//! 1. **A manifest entry is a fact.** `['busted.core'] = 'busted/core.lua'`
//!    in a rockspec's `build.modules` names one file, and nothing about
//!    `package.path` can change what the rock ships. Asked first, and it is
//!    the only rule here that does not rest on a convention.
//! 2. **Otherwise, `package.path`'s own two patterns, against the repository
//!    root.** `?.lua` gives `busted/core.lua` for `busted.core`, and
//!    `?/init.lua` gives `busted/core/init.lua`. Both are probed, always —
//!    not because a resolver needs a second chance, but because *how many of
//!    them exist* is the answer:
//!    - exactly one exists → that is the module, and it resolves;
//!    - **both exist → the tree does not say which**, and the reference is
//!      [`UnresolvedReason::ProjectLayoutUnknown`];
//!    - neither exists → [`UnresolvedReason::ModuleNotFound`].
//! 3. **A specifier that is not one plain literal resolves against nothing.**
//!    [`UnresolvedReason::DynamicModuleSpecifier`], never a guess:
//!    `require('busted.outputHandlers.' .. output)` names a file only the
//!    running program knows, and the value comes from a command-line option.
//!
//! # Why two candidates is a layout failure and not an ambiguous export
//!
//! The measured corpus has exactly one such module and it is its own name:
//! `busted` matches `busted.lua` under `?.lua` and `busted/init.lua` under
//! `?/init.lua`, at **54 call sites**. Lua does not call that ambiguous — it
//! searches `package.path` in order and deterministically loads whichever
//! pattern comes first — so [`UnresolvedReason::AmbiguousExport`], whose own
//! definition requires that *the language* call the result ambiguous, does
//! not describe it. What is unknown is the search path, and the corpus proves
//! the search path is not a property of the tree twice over: the root file
//! exists only "so it can be used in busted's specs without adding
//! `./?/init.lua` to the lua path", and `busted/runner.lua` prepends the
//! `--lpath` argument to `package.path` before anything is required.
//!
//! [`UnresolvedReason::ProjectLayoutUnknown`] says exactly that, in both of
//! its clauses: the layout could not be determined, and **the failure is
//! arthron's own inference rather than a missing definition** — both files
//! are right here in the graph. It is the same call Python's resolver makes
//! for a module reached under a mutated `sys.path`, and `package.path` is
//! `sys.path`.
//! *Rejected:* picking one silently, which is right about half the time and
//! says so nowhere.
//!
//! # Why `External` is never minted
//!
//! A rockspec declares *rock* names. A rock name is not a module name, and
//! the measured corpus refutes the identification six times out of nine —
//! `penlight` ships `pl.*`, `lua-term` ships `term`, `lua_cliargs` ships
//! `cliargs`, `mediator_lua` ships `mediator`, `luasystem` ships `system`,
//! and `lua` ships the standard library. So `require 'pl.path'` and
//! `require 'say'` alike end [`UnresolvedReason::ModuleNotFound`] and count
//! *against* the rate.
//!
//! That is the deliberately expensive answer. `External` sits outside *both*
//! terms of the resolution rate, so widening it is the cheapest way there is
//! to raise a rate without linking anything — and a rock-to-module table
//! written from ecosystem familiarity rather than measured is exactly how
//! that widening would arrive. A track that mints no `External` cannot raise
//! its rate by reclassifying.
//!
//! # Why the miss reason is `ModuleNotFound` and not `UnknownPackage`
//!
//! `UnknownPackage` asserts that the target names a package **outside the
//! repository**. This corpus disproves that for three of its own sites:
//! `require 'cl_test_module'` names `spec/cl_test_module.lua`, a file that is
//! right here, addressed under the module name it has when the runner starts
//! in `spec/` — and the same file is required as `spec.cl_test_module` from
//! the root, where it resolves. Nothing in the text tells that case apart
//! from `require 'pl.path'`. `ModuleNotFound` — *the specifier is a literal
//! and resolved to no module under the configured resolution* — is true of
//! both without asserting where either lives, and a reason that is never
//! wrong beats a reason that is usually right.
//!
//! # `LocalBinding` does not apply here
//!
//! Tier 2 emits no expression-level reference, so no Lua reference can name a
//! parameter, a local or an upvalue. The bucket stays empty, and the baseline
//! records it as zero — which, with `external` also zero, makes this track's
//! rate un-gameable by either reclassification the rate's own definition
//! permits.

use std::collections::HashMap;
use std::path::Path;

use crate::UnresolvedReason;
use crate::lang::{FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe};
use crate::model::{
    DefFacets, DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, Reference, node_id,
};
use crate::track_lua::extract::{ImportForm, LuaExtractor, LuaHeader};
use crate::track_lua::lang::{LuaLang, chunk_fqn, member_fqn};
use crate::track_lua::project::{LuaProject, layout};

/// One file's view of what its own `require` sites mean.
///
/// One fact and no more: what each site spells, keyed by the span the site
/// shares with its reference. Lua has no relative import, so unlike Ruby's
/// scope this one does not need to know where the file sits.
pub struct LuaScope {
    /// Each import site's form, by `(byte_start, byte_end)` of its call.
    imports: HashMap<(u32, u32), ImportForm>,
}

/// An outcome with nothing probed.
fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: crate::Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// A repo-relative path with `.` and `..` resolved, or `None` when it would
/// escape the repository root.
///
/// A path this scan cannot see is not one it may claim to have found.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The path a module name spells, `package.path`'s `?` substitution: every
/// `.` becomes a directory separator.
///
/// `None` when the name spells no path at all — an empty name, or one whose
/// dots leave nothing between them. `require 'busted.outputHandlers.'`, the
/// literal half of a concatenated specifier, never reaches here: it is
/// [`ImportForm::Dynamic`] before any of this runs.
fn module_path(name: &str) -> Option<String> {
    if name.is_empty() || name.split('.').any(str::is_empty) {
        return None;
    }
    normalize(&name.replace('.', "/"))
}

/// The identity of the chunk a repo-relative path is.
fn chunk_id(path: &str) -> NodeId {
    node_id(Domain::Lua, &chunk_fqn(path))
}

/// Lua's resolver. Stateless: everything it reads is in the config, the
/// scope, or the probe.
pub struct LuaResolver;

impl LuaResolver {
    /// One literal module name, against the manifest and then against
    /// `package.path`'s own patterns.
    fn module(cfg: &LuaProject, name: &str, probe: &dyn SymbolProbe) -> Resolution {
        // Every identity read, hits and misses, in read order: the
        // invalidation index is built from it, so adding `busted/init.lua` to
        // a tree wakes every `require 'busted'` that resolved without it.
        let mut candidates: Vec<NodeId> = Vec::new();

        // 1. The manifest, which states a fact rather than a convention.
        if let Some(path) = cfg.declared_module(name).and_then(normalize) {
            let id = chunk_id(&path);
            candidates.push(id);
            if probe.probe(&id).is_some() {
                return Resolution {
                    outcome: crate::Outcome::Resolved(id),
                    candidates,
                };
            }
        }

        // 2. `?.lua` and `?/init.lua`. Both are probed, always: how many of
        //    them exist is the answer, not which one is found first.
        let Some(base) = module_path(name) else {
            return Resolution {
                outcome: crate::Outcome::Unresolved(UnresolvedReason::ModuleNotFound),
                candidates,
            };
        };
        let mut found: Vec<NodeId> = Vec::new();
        for path in [base.clone(), format!("{base}/init")] {
            let id = chunk_id(&path);
            if !candidates.contains(&id) {
                candidates.push(id);
            }
            if probe.probe(&id).is_some() && !found.contains(&id) {
                found.push(id);
            }
        }
        let outcome = match found.as_slice() {
            [one] => crate::Outcome::Resolved(*one),
            // Both patterns name a file that is here. Which one `require`
            // loads is decided by the order of the patterns in
            // `package.path` at run time, which the tree does not state.
            [_, _, ..] => crate::Outcome::Unresolved(UnresolvedReason::ProjectLayoutUnknown),
            [] => crate::Outcome::Unresolved(UnresolvedReason::ModuleNotFound),
        };
        Resolution {
            outcome,
            candidates,
        }
    }
}

impl Resolver<LuaLang> for LuaResolver {
    fn config(&self, root: &Path, _files: &FileIndex) -> Result<LuaProject, LayoutError> {
        layout(root)
    }

    fn config_digest(&self, cfg: &LuaProject) -> Vec<u8> {
        // The module map names the file every declared module is, so a scan
        // under a different one describes a different graph and cannot be
        // patched into this one file by file.
        cfg.digest()
    }

    fn declared_container(
        &self,
        _cfg: &LuaProject,
        _header: &LuaHeader,
    ) -> Option<(String, String)> {
        // A Lua file names no container for anybody else. The chunk a file
        // *is* comes from its path — or from a manifest entry naming that
        // path — never from its source.
        None
    }

    fn learn_containers(&self, _cfg: &mut LuaProject, _names: &HashMap<String, String>) {
        // Nothing a Lua reference binds is derived from another file's
        // source, so there is nothing to learn.
    }

    fn owns_file(&self, _cfg: &LuaProject, _rel_path: &str) -> bool {
        // No nested-manifest fence: a rockspec below the repository root is a
        // shape phase 0 does not read, so no file is excluded on account of
        // one.
        true
    }

    fn def_fqn(
        &self,
        _cfg: &LuaProject,
        header: &LuaHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        let chunk = chunk_fqn(&header.rel_path);
        // The file's own chunk node: synthesized, at the top level, and a
        // module. Its identity is the path, because that is what `require`
        // spells and what `package.loaded` holds.
        if def.kind == DefKind::Module
            && def.facets.contains(DefFacets::SYNTHETIC)
            && owner.is_empty()
        {
            return Some(Fqn::new(chunk));
        }
        let mut path = owner.to_vec();
        path.push(def.name.clone());
        member_fqn(&chunk, &path).map(Fqn::new)
    }

    fn index_keys(&self, _cfg: &LuaProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // Every Lua node is reachable by exactly one identity: a chunk by its
        // path, a member by its path under that chunk.
        Vec::new()
    }

    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        // `function M.foo()` written twice in one chunk writes one table key
        // twice, and there is one `M.foo` at run time. Two `local function
        // helper` in two closures of one chunk are genuinely two functions,
        // and this grammar cannot tell them apart without tracking Lua's
        // local scope — so they merge too, and the corpus acceptance pins the
        // extractor's census beside the store's so that the size of that
        // merge is a recorded number rather than a silent one. Neither case
        // is the corruption the collision count exists to surface.
        a.kind == b.kind && a.name == b.name && a.owner == b.owner
    }

    fn scope(
        &self,
        _cfg: &LuaProject,
        file: &FileFacts<LuaLang>,
        _probe: &dyn SymbolProbe,
    ) -> LuaScope {
        LuaScope {
            imports: file
                .header
                .imports
                .iter()
                .map(|i| ((i.span.byte_start, i.span.byte_end), i.form.clone()))
                .collect(),
        }
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // Lua has no declared supertype: `setmetatable(C, { __index = Base })`
        // is a call on runtime values, and tier 2 emits no call reference. So
        // there is no relation to build and nothing for the driver to run a
        // phase over.
        &[]
    }

    fn resolve(
        &self,
        cfg: &LuaProject,
        scope: &LuaScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        match scope.imports.get(&(r.span.byte_start, r.span.byte_end)) {
            Some(ImportForm::Module(name)) => Self::module(cfg, name, probe),
            // A specifier that could not be read as one literal, and —
            // unreachable, since the extractor emits a site and its reference
            // together — a reference with no site at all. Both mean the same
            // thing: this build cannot say which file is named, and it will
            // not guess one.
            Some(ImportForm::Dynamic) | None => {
                unresolved(UnresolvedReason::DynamicModuleSpecifier)
            }
        }
    }
}

/// The Lua track's scan entry point, reading every `.lua` the walk finds.
pub fn scan_lua(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_lua_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_lua`] under a repository's include/exclude globs. What
/// [`crate::track_lua::TRACK`] holds.
pub fn scan_lua_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<LuaLang>(root, db, &LuaExtractor, &LuaResolver, filter)
}

/// Lua's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(LuaLang::LANG, Lang::Lua));
    assert!(matches!(LuaLang::DOMAIN, Domain::Lua));
};
