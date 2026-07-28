//! Resolver fixtures for Lua: one import model, one outcome per reference.
//!
//! The import model these pin is the one measured on the corpus: a rockspec's
//! `build.modules` entry is a fact and is asked first, `package.path`'s own
//! `?.lua` and `?/init.lua` patterns are a convention and are asked second —
//! **both of them, always**, because how many of them exist is the answer —
//! and a specifier that is not one plain literal resolves against nothing and
//! says so.

use std::collections::HashSet;

use arthron::UnresolvedReason::{self, *};
use arthron::lang::Resolver;
use arthron::model::{Domain, NodeId, node_id};
use arthron::track_lua::extract::extract;
use arthron::track_lua::lang::chunk_fqn;
use arthron::track_lua::project::LuaProject;
use arthron::track_lua::resolve::LuaResolver;
use arthron::{Outcome, resolution_rate};

/// A project whose manifest states this module map.
fn project(modules: &[(&str, &str)]) -> LuaProject {
    LuaProject {
        modules: modules
            .iter()
            .map(|(m, f)| ((*m).to_string(), (*f).to_string()))
            .collect(),
        rockspecs: vec!["p-1.rockspec".to_string()],
    }
}

/// A symbol table holding the chunk node of every one of these files.
fn table(files: &[&str]) -> HashSet<NodeId> {
    files
        .iter()
        .map(|f| node_id(Domain::Lua, &chunk_fqn(f)))
        .collect()
}

/// Resolve every reference one file's source produces.
fn outcomes(
    cfg: &LuaProject,
    known: &HashSet<NodeId>,
    rel: &str,
    source: &str,
) -> Vec<Outcome<NodeId, String>> {
    let facts = extract(rel, source);
    let scope = LuaResolver.scope(cfg, &facts, known);
    facts
        .refs
        .iter()
        .map(|r| LuaResolver.resolve(cfg, &scope, r, known).outcome)
        .collect()
}

/// The single outcome one `require` produces.
fn one(cfg: &LuaProject, known: &HashSet<NodeId>, source: &str) -> Outcome<NodeId, String> {
    let mut found = outcomes(cfg, known, "busted/app.lua", source);
    assert_eq!(found.len(), 1, "expected exactly one reference");
    found.remove(0)
}

/// The reason one `require` failed with.
fn reason(cfg: &LuaProject, known: &HashSet<NodeId>, source: &str) -> UnresolvedReason {
    match one(cfg, known, source) {
        Outcome::Unresolved(r) => r,
        other => panic!("expected an unresolved outcome, got {other:?}"),
    }
}

#[test]
fn a_dotted_module_name_is_a_path_under_the_question_mark_pattern() {
    let cfg = project(&[]);
    let known = table(&["busted/core.lua"]);
    assert_eq!(
        one(&cfg, &known, "require 'busted.core'\n"),
        Outcome::Resolved(node_id(Domain::Lua, "$busted/core")),
    );
}

#[test]
fn the_init_pattern_answers_when_the_plain_one_does_not() {
    let cfg = project(&[]);
    let known = table(&["busted/init.lua"]);
    assert_eq!(
        one(&cfg, &known, "require 'busted'\n"),
        Outcome::Resolved(node_id(Domain::Lua, "$busted/init")),
    );
}

#[test]
fn two_patterns_that_both_name_a_file_are_a_layout_this_tree_does_not_state() {
    // busted's own shape, at 54 call sites: `busted.lua` matches `?.lua` and
    // `busted/init.lua` matches `?/init.lua`. Which one `require` loads is
    // decided by the order of the patterns in `package.path` when the process
    // starts, and the corpus proves that order is not a property of the tree.
    let cfg = project(&[]);
    let known = table(&["busted.lua", "busted/init.lua"]);
    assert_eq!(
        reason(&cfg, &known, "require 'busted'\n"),
        ProjectLayoutUnknown,
    );
    // Both candidates are recorded, so adding or removing either wakes this
    // reference — and so a reader can see what the two answers were.
    let facts = extract("busted/app.lua", "require 'busted'\n");
    let scope = LuaResolver.scope(&cfg, &facts, &known);
    let probed = LuaResolver
        .resolve(&cfg, &scope, &facts.refs[0], &known)
        .candidates;
    assert_eq!(
        probed,
        [
            node_id(Domain::Lua, "$busted"),
            node_id(Domain::Lua, "$busted/init"),
        ],
    );
}

#[test]
fn the_manifest_wins_over_the_convention_and_is_asked_first() {
    // A rock whose sources live under `src/` is the ordinary LuaRocks shape,
    // and the convention alone answers nothing for it. This is the one rule
    // here that rests on a stated fact rather than on `package.path`.
    let cfg = project(&[("busted.core", "src/core.lua")]);
    let known = table(&["src/core.lua", "busted/core.lua"]);
    assert_eq!(
        one(&cfg, &known, "require 'busted.core'\n"),
        Outcome::Resolved(node_id(Domain::Lua, "$src/core")),
    );
}

#[test]
fn a_manifest_entry_naming_a_file_that_is_not_here_falls_through_to_the_convention() {
    // The manifest may name a file the walk never read — one under a hidden
    // directory, or one this snapshot excluded. A miss there is not an answer.
    let cfg = project(&[("busted.core", "generated/core.lua")]);
    let known = table(&["busted/core.lua"]);
    assert_eq!(
        one(&cfg, &known, "require 'busted.core'\n"),
        Outcome::Resolved(node_id(Domain::Lua, "$busted/core")),
    );
}

#[test]
fn a_literal_that_names_no_module_here_is_module_not_found_and_never_external() {
    // `require 'pl.path'` names Penlight, and `require 'cl_test_module'`
    // names a file that is right here under another root. Nothing in the text
    // tells them apart, and `ModuleNotFound` is true of both without
    // asserting where either lives.
    let cfg = project(&[]);
    let known = table(&["spec/cl_test_module.lua"]);
    assert_eq!(reason(&cfg, &known, "require 'pl.path'\n"), ModuleNotFound);
    assert_eq!(
        reason(&cfg, &known, "require 'cl_test_module'\n"),
        ModuleNotFound,
    );
    // The same file under the module name the repository root does give it.
    assert_eq!(
        one(&cfg, &known, "require 'spec.cl_test_module'\n"),
        Outcome::Resolved(node_id(Domain::Lua, "$spec/cl_test_module")),
    );
}

#[test]
fn a_declared_rock_name_is_not_an_external_node() {
    // The manifest declares rock names, and a rock name is not a module
    // name — this corpus refutes the identification six times out of nine.
    // `External` sits outside both rate terms, so a track that mints none
    // cannot raise its rate by reclassifying.
    let cfg = project(&[]);
    let known = table(&["busted/core.lua"]);
    for spec in ["say", "pl.path", "luassert.stub", "term.colors", "string"] {
        let source = format!("require '{spec}'\n");
        match one(&cfg, &known, &source) {
            Outcome::Unresolved(ModuleNotFound) => {}
            other => panic!("{spec}: {other:?}"),
        }
    }
}

#[test]
fn a_specifier_that_is_not_one_literal_is_never_guessed() {
    let cfg = project(&[]);
    let known = table(&["busted/languages/en.lua", "busted/outputHandlers/base.lua"]);
    for source in [
        "require('busted.languages.' .. options.language)\n",
        "local fn = require(helper)\n",
        "require(('busted.%s'):format(n))\n",
    ] {
        assert_eq!(
            reason(&cfg, &known, source),
            DynamicModuleSpecifier,
            "{source}"
        );
    }
}

#[test]
fn a_name_that_spells_no_path_resolves_to_nothing_rather_than_to_the_root() {
    // The literal half of a concatenated specifier never reaches the
    // resolver, but a hand-written `require '.'` or `require 'a..b'` would,
    // and neither may be allowed to name the repository root.
    let cfg = project(&[]);
    let known = table(&["busted/core.lua", "init.lua"]);
    for spec in ["", ".", "a..b", "busted."] {
        let source = format!("require '{spec}'\n");
        assert_eq!(reason(&cfg, &known, &source), ModuleNotFound, "{spec:?}");
    }
}

#[test]
fn the_rate_counts_what_it_says_it_counts() {
    let cfg = project(&[]);
    let known = table(&["busted/core.lua", "busted.lua", "busted/init.lua"]);
    let found = outcomes(
        &cfg,
        &known,
        "busted/app.lua",
        "require 'busted.core'\nrequire 'busted'\nrequire 'say'\nrequire(x)\n",
    );
    let resolved = found.iter().filter(|o| o.is_resolved()).count() as u64;
    let unresolved = found
        .iter()
        .filter(|o| o.unresolved_reason().is_some())
        .count() as u64;
    assert_eq!((resolved, unresolved), (1, 3));
    // No `External` and no `LocalBinding`: every reference is in one of the
    // two terms, so nothing sits outside the rate at all.
    assert!(!found.iter().any(|o| matches!(o, Outcome::External(_))));
    assert!(
        !found
            .iter()
            .any(|o| o.unresolved_reason() == Some(&LocalBinding))
    );
    assert_eq!(resolution_rate(resolved, unresolved), Some(0.25));
}

#[test]
fn every_import_reference_is_paired_with_a_site() {
    // The pairing is by span, so a reference the scope cannot find would
    // silently become `DynamicModuleSpecifier` for a perfectly literal
    // specifier. It must be total.
    let cfg = project(&[]);
    let known = table(&["busted/core.lua"]);
    let source = "require 'busted.core'\nlocal ok = pcall(require, 'moonscript')\n\
                  function M.f()\n  require(x)\nend\n";
    let facts = extract("busted/app.lua", source);
    assert_eq!(facts.refs.len(), 3);
    assert_eq!(facts.header.imports.len(), 3);
    let outcomes = outcomes(&cfg, &known, "busted/app.lua", source);
    assert_eq!(
        outcomes,
        [
            Outcome::Resolved(node_id(Domain::Lua, "$busted/core")),
            Outcome::Unresolved(ModuleNotFound),
            Outcome::Unresolved(DynamicModuleSpecifier),
        ],
    );
}

#[test]
fn a_chunk_and_a_member_of_it_are_two_identities_that_cannot_collide() {
    let cfg = project(&[]);
    let known: HashSet<NodeId> = HashSet::new();
    let facts = extract("busted/block.lua", "function block.reject() end\n");
    let chunk = LuaResolver
        .def_fqn(&cfg, &facts.header, &[], &facts.defs[0], &known)
        .expect("every file is a chunk");
    let member = LuaResolver
        .def_fqn(
            &cfg,
            &facts.header,
            &facts.defs[1].owner,
            &facts.defs[1],
            &known,
        )
        .expect("a named member");
    assert_eq!(chunk.as_str(), "$busted/block");
    assert_eq!(member.as_str(), "$busted/block#block.reject");
    assert_ne!(chunk, member);
    // `:` is reserved for the `external:` prefix and appears in neither.
    assert!(!member.as_str().contains(':'));
}
