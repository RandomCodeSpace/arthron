//! Extractor fixtures for Lua, one construct at a time.
//!
//! Lua is a **tier-2** track: definitions, structure, and imports. The
//! reference kinds asserted here are therefore only [`RefKind::Import`] — a
//! track that emitted calls un-gated would report tier-1 coverage it has not
//! measured, and in Lua that line is uniquely easy to cross because `require`
//! *is* a call. Every fixture that writes another call writes it precisely to
//! assert that nothing comes back.

use arthron::model::{DeclSpace, DefFacets, DefKind, RefKind, TargetRoot};
use arthron::track_lua::extract::{ImportForm, extract};

/// Every definition as `(kind, owner joined, name)`, in emission order.
fn defs(source: &str) -> Vec<(DefKind, String, String)> {
    extract("busted/app.lua", source)
        .defs
        .into_iter()
        .map(|d| (d.kind, d.owner.join("."), d.name))
        .collect()
}

/// Every definition below the file's own chunk node.
fn members(source: &str) -> Vec<(DefKind, String, String)> {
    defs(source).into_iter().skip(1).collect()
}

/// Every import as `(form, raw target)`, in source order.
fn imports(source: &str) -> Vec<(ImportForm, String)> {
    let facts = extract("busted/app.lua", source);
    facts
        .header
        .imports
        .iter()
        .zip(&facts.refs)
        .map(|(spec, r)| (spec.form.clone(), r.raw_target.clone()))
        .collect()
}

#[test]
fn every_file_declares_the_chunk_a_require_names() {
    // First, because the driver reads the first `Module` definition as the
    // file's container — and present even when the file declares nothing
    // else, because a `require` naming an empty file still resolves.
    let facts = extract("busted/core.lua", "");
    assert_eq!(facts.defs.len(), 1);
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].name, "core");
    assert_eq!(facts.defs[0].space, DeclSpace::Namespace);
    assert!(facts.defs[0].facets.contains(DefFacets::SYNTHETIC));
    assert!(facts.defs[0].owner.is_empty());
}

#[test]
fn a_broken_file_still_yields_its_chunk_node() {
    // tree-sitter is error-tolerant, and a file that does not parse is still
    // a file a `require` can name.
    let facts = extract("busted/broken.lua", "function (((\n");
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].name, "broken");
}

#[test]
fn a_function_declaration_is_a_function_and_local_does_not_change_that() {
    // `function f()` writes `_G.f` and `local function f()` writes a local,
    // unless some enclosing block already wrote `local f` — telling those
    // apart needs scope tracking this track does not do, and both are one
    // definition of this chunk either way.
    assert_eq!(
        members("function glob() end\nlocal function loc() end\n"),
        [
            (DefKind::Function, String::new(), "glob".to_string()),
            (DefKind::Function, String::new(), "loc".to_string()),
        ],
    );
}

#[test]
fn a_function_on_a_table_is_a_method_and_carries_the_path_as_written() {
    assert_eq!(
        members("function M.foo() end\nfunction a.b.c() end\n"),
        [
            (DefKind::Method, "M".to_string(), "foo".to_string()),
            (DefKind::Method, "a.b".to_string(), "c".to_string()),
        ],
    );
}

#[test]
fn the_colon_form_and_the_dot_form_write_one_key_and_are_one_node() {
    // `function M:bar()` is sugar for `M.bar = function(self) ... end`: the
    // very same table key. Two nodes here would be one slot counted twice.
    let colon = members("function M:bar() end\n");
    let dot = members("function M.bar() end\n");
    assert_eq!(colon, dot);
    assert_eq!(
        colon,
        [(DefKind::Method, "M".to_string(), "bar".to_string())]
    );
}

#[test]
fn a_function_valued_assignment_declares_and_a_data_one_does_not() {
    assert_eq!(
        members(
            "local M = {}\nM.baz = function() end\nM.qty = 3\n\
             local helper = function() end\nlocal n = 1\n",
        ),
        [
            (DefKind::Method, "M".to_string(), "baz".to_string()),
            (DefKind::Function, String::new(), "helper".to_string()),
        ],
    );
}

#[test]
fn a_multi_target_assignment_pairs_by_position() {
    // The grammar's `name`/`value` fields answer only for the first target,
    // so a resolver reading them would file `g`'s body under `f`.
    assert_eq!(
        members("local f, g = 1, function() end\n"),
        [(DefKind::Function, String::new(), "g".to_string())],
    );
}

#[test]
fn a_named_table_composes_its_owner_through_every_level() {
    // The nested-container case: `c`'s owner is the whole chain that names
    // the table it sits in, not just the entry directly above it.
    assert_eq!(
        members("local M = { a = { b = { c = function() end } } }\n"),
        [(DefKind::Method, "M.a.b".to_string(), "c".to_string())],
    );
    assert_eq!(
        members("M.a = { b = function() end }\n"),
        [(DefKind::Method, "M.a".to_string(), "b".to_string())],
    );
}

#[test]
fn the_chunks_own_return_table_declares_its_members_directly() {
    // `busted/compatibility.lua`'s shape: the module's API is a table
    // literal the chunk returns, and it has no name to carry — so `exit` is
    // exactly what `require 'busted.compatibility'.exit` names.
    assert_eq!(
        members("return {\n  exit = function() end,\n  standalone = true,\n}\n"),
        [(DefKind::Function, String::new(), "exit".to_string())],
    );
}

#[test]
fn a_table_with_no_name_declares_nothing() {
    // Returned from inside a function, or passed as an argument: the owner
    // is a value this file does not name, so no node is invented for it.
    assert_eq!(
        members("local function save()\n  return { g = function() end }\nend\n"),
        [(DefKind::Function, String::new(), "save".to_string())],
    );
    assert!(members("subscribe({ handler = function() end })\n").is_empty());
}

#[test]
fn a_key_that_is_not_a_name_declares_nothing() {
    assert!(members("local h = { [1] = function() end, [k] = function() end }\n").is_empty());
    assert!(members("t[k] = function() end\n").is_empty());
}

#[test]
fn a_quoted_key_and_a_bare_key_name_the_same_member() {
    assert_eq!(
        members("local M = { ['alpha'] = function() end }\n"),
        members("local M = { alpha = function() end }\n"),
    );
}

#[test]
fn a_declaration_inside_a_closure_is_still_a_declaration_of_its_chunk() {
    // busted's whole library is written as `return function(busted) ... end`
    // factories. Skipping what they declare would leave the census empty and
    // the tier-2 deliverable unmeasured.
    assert_eq!(
        members(
            "return function(busted)\n  local block = {}\n  function block.reject() end\nend\n"
        ),
        [(DefKind::Method, "block".to_string(), "reject".to_string())],
    );
}

#[test]
fn a_field_whose_value_only_yields_a_function_is_not_read() {
    // `getfenv = getfenv or function(f) ... end` declares the module's
    // `getfenv`, and reading it would mean deciding which operand a runtime
    // test picks. A recorded under-count, asserted so it stays one.
    assert!(members("return { getfenv = getfenv or function(f) end }\n").is_empty());
}

#[test]
fn require_is_the_only_call_that_becomes_a_reference() {
    let facts = extract(
        "busted/app.lua",
        "local s = require 'say'\ns:set('a', 'b')\nbusted.subscribe({'suite'}, handler)\nprint(1)\n",
    );
    assert_eq!(facts.refs.len(), 1);
    assert_eq!(facts.refs[0].kind, RefKind::Import);
    assert_eq!(facts.refs[0].space, DeclSpace::Namespace);
    assert_eq!(facts.refs[0].target.root, TargetRoot::Name);
    assert_eq!(facts.refs[0].target.segments, ["say"]);
    assert!(!facts.refs[0].locally_bound);
    assert_eq!(facts.refs[0].argc, None);
}

#[test]
fn every_call_spelling_of_require_is_one_import() {
    assert_eq!(
        imports("require('a')\nrequire 'b'\nrequire [[c]]\nrequire(\"d\")\n"),
        [
            (ImportForm::Module("a".into()), "require('a')".to_string()),
            (ImportForm::Module("b".into()), "require 'b'".to_string()),
            (ImportForm::Module("c".into()), "require [[c]]".to_string()),
            (ImportForm::Module("d".into()), "require(\"d\")".to_string()),
        ],
    );
}

#[test]
fn an_optional_dependency_through_pcall_is_still_an_import() {
    // Leaving these out would *raise* the rate by deleting references that
    // miss, which is the one direction an omission must never go.
    assert_eq!(
        imports("local ok, m = pcall(require, 'moonscript')\n"),
        [(
            ImportForm::Module("moonscript".into()),
            "pcall(require, 'moonscript')".to_string(),
        )],
    );
    // `pcall` around anything else is an ordinary call and contributes
    // nothing; the inner `require` inside a closure is its own site.
    assert_eq!(
        imports("pcall(function() require 'z' end)\n"),
        [(ImportForm::Module("z".into()), "require 'z'".to_string())],
    );
}

#[test]
fn a_specifier_that_is_not_one_plain_literal_is_dynamic() {
    let found = imports(
        "require('busted.languages.' .. options.language)\nlocal fn = require(helper)\n\
         require('a' .. 'b')\nrequire 'esc\\116'\n",
    );
    assert_eq!(
        found.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>(),
        [
            ImportForm::Dynamic,
            ImportForm::Dynamic,
            ImportForm::Dynamic,
            ImportForm::Dynamic,
        ],
    );
    // The target of a dynamic site is not a name, which is exactly the shape
    // `TargetRoot::Expr` exists for.
    let facts = extract("busted/app.lua", "require(helper)\n");
    assert_eq!(facts.refs[0].target.root, TargetRoot::Expr);
    assert!(facts.refs[0].target.segments.is_empty());
}

#[test]
fn require_with_no_argument_is_not_an_import_site() {
    // There is no module named, so there is nothing to fail to resolve, and
    // a reference here would be a denominator entry nothing could ever move.
    assert!(imports("require()\n").is_empty());
}

#[test]
fn a_require_whose_result_is_indexed_or_called_is_still_one_import() {
    // `require 'busted.compatibility'.getfenv` and
    // `require 'busted.block'(busted)` are both written in the corpus.
    assert_eq!(
        imports("local g = require 'busted.compatibility'.getfenv\n"),
        [(
            ImportForm::Module("busted.compatibility".into()),
            "require 'busted.compatibility'".to_string(),
        )],
    );
    assert_eq!(
        imports("local b = require 'busted.block'(busted)\n"),
        [(
            ImportForm::Module("busted.block".into()),
            "require 'busted.block'".to_string(),
        )],
    );
}

#[test]
fn an_import_is_filed_under_the_nearest_nameable_encloser() {
    let facts = extract(
        "busted/app.lua",
        "require 'top'\nfunction M.foo()\n  require 'inner'\nend\n\
         it('x', function() require 'anon' end)\n",
    );
    let enclosing: Vec<Option<Vec<String>>> = facts
        .refs
        .iter()
        .map(|r| r.enclosing.as_ref().map(|e| e.path.clone()))
        .collect();
    assert_eq!(
        enclosing,
        [
            // The chunk's top level names nothing; the driver sources it at
            // the file's own chunk node.
            None,
            Some(vec!["M".to_string(), "foo".to_string()]),
            // An anonymous function is stepped over, not stopped at.
            None,
        ],
    );
    assert_eq!(
        facts.refs[1].enclosing.as_ref().map(|e| e.kind),
        Some(DefKind::Method),
    );
}

#[test]
fn records_come_out_in_source_order_and_pair_one_to_one() {
    let facts = extract(
        "busted/app.lua",
        "require 'a'\nlocal M = {}\nfunction M.go() end\nrequire 'b'\n",
    );
    assert_eq!(
        facts.refs.iter().map(|r| r.span.line).collect::<Vec<_>>(),
        [1, 4],
    );
    assert!(
        facts
            .defs
            .windows(2)
            .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
        "{:?}",
        facts.defs,
    );
    // An import site and its reference are paired by span, so a site with no
    // reference would be a silently dropped import.
    assert_eq!(facts.header.imports.len(), facts.refs.len());
    for (spec, r) in facts.header.imports.iter().zip(&facts.refs) {
        assert_eq!(spec.span, r.span);
    }
}
