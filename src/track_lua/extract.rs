//! Lua extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/lua.yml`) select nodes by kind; this
//! module interprets their fields.
//!
//! # What a tier-2 extractor emits, and what it must not
//!
//! Definitions and structure, plus **import references and nothing else**.
//! Lua's gate is an import-resolution rate, so a call site emitted here would
//! enter a denominator nothing in this track resolves — tier-1 coverage
//! claimed without tier-1 measurement. That bites harder in Lua than
//! anywhere else on this list: `require` *is* an ordinary function call, so
//! the rule "no call references" and the rule "import references" are a
//! distinction this extractor has to draw by the callee's name.
//!
//! # The two shapes that import
//!
//! - `require <specifier>`, in any of Lua's three call spellings —
//!   `require(x)`, `require 'x'`, `require [[x]]`.
//! - `pcall(require, <specifier>)`, the idiom for an optional dependency.
//!   Seven sites in the measured corpus, all naming rocks outside it. They
//!   are read because leaving them out would *raise* the rate by deleting
//!   references that miss — the one direction an omission must never go.
//!
//! `xpcall(require, handler, <specifier>)` is the same idiom with a message
//! handler and is **not** read: no site in the measured corpus writes it, and
//! a shape nothing exercises is recorded here rather than implemented blind.
//!
//! # Recorded under-counts
//!
//! Each is a known shortfall, written down rather than left to be
//! rediscovered, and none may be closed by widening a bucket:
//!
//! - **A field whose value only *yields* a function.** `getfenv = getfenv or
//!   function(f) ... end` declares the module's `getfenv`, and the value node
//!   is a binary expression rather than a function. Reading it would mean
//!   deciding which operand a runtime test picks.
//! - **A non-function field.** `M.qty = 3` and `options = { standalone =
//!   true }` are data on a table, and `self.foo = x` inside a method body is
//!   a write rather than a declaration. One node per assignment would be
//!   several nodes for one slot, which is the same call Ruby's instance
//!   variables get.
//! - **The module table itself.** `local M = {}` binds a local, and a local
//!   is not a node by decision. Its function-valued keys are, because another
//!   file really can name one through the value the chunk returns.
//! - **A global is filed under its chunk.** `function f()` writes `_G.f`
//!   unless some enclosing block wrote `local f` first. Telling those apart
//!   needs local-scope tracking; naming both under the chunk needs none and
//!   claims less.
//! - **A key that is not a name.** `h[1] = function() end` and `t[k] =
//!   function() end` write a slot no lexical path spells.
//! - **A table with no name.** A table literal passed as an argument, or
//!   returned from inside a function body, has no owner this file states, so
//!   its function-valued fields produce no node rather than a guessed one.
//!   A table literal returned by the **chunk** does have one — it is the
//!   chunk's own export table — and its fields are filed directly under the
//!   chunk.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_lua::lang::LuaLang;

/// The embedded Lua extraction rules.
const LUA_RULES: &str = include_str!("../rules/lua.yml");

/// How an import site spells the module it names.
///
/// The distinction is the whole of Lua's import model: a literal names a
/// module `package.path` can be searched for, and anything else names a
/// module only the running program knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportForm {
    /// A plain string literal: the module name as written.
    Module(String),
    /// The specifier is not one plain string literal — concatenated,
    /// computed, or a name. Never guessed.
    Dynamic,
}

/// One import site: what it spells plus where it sits.
///
/// Every `ImportSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is how the
/// resolver pairs the two without the core learning what a `require` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    /// What the site spells.
    pub form: ImportForm,
    /// Where the site sits. The whole call, so the key is unique.
    pub span: Span,
}

/// Per-file Lua facts only the Lua resolver reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LuaHeader {
    /// Repo-relative, `/`-separated path of the file.
    pub rel_path: String,
    /// Every import site, in source order.
    pub imports: Vec<ImportSpec>,
}

/// A string literal's value, or `None` when it is not one plain literal.
///
/// `pub(crate)` because phase 0 reads the same shape out of a rockspec — one
/// reader for "is this a plain literal string?", not two that can drift.
///
/// A backslash answers `None`. This build decodes no Lua escape sequence, and
/// a specifier it cannot read exactly is one the resolver must refuse to
/// guess rather than one it may approximate. Long-bracket strings carry no
/// escapes at all, so the check costs them nothing a module name would use.
pub(crate) fn string_literal(node: &SgNode) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut out = String::new();
    for child in node.children() {
        match &*child.kind() {
            "string_content" => out.push_str(&child.text()),
            // The delimiters: `'`, `"`, and the long-bracket pair, which
            // tree-sitter-lua names `[[` and `]]` at every level.
            "'" | "\"" | "[[" | "]]" => {}
            _ => return None,
        }
    }
    (!out.contains('\\')).then_some(out)
}

/// A call's arguments, punctuation dropped.
pub(crate) fn arg_nodes<'r>(call: &SgNode<'r>) -> Vec<SgNode<'r>> {
    let Some(list) = call.field("arguments") else {
        return Vec::new();
    };
    list.children()
        .filter(|c| !matches!(&*c.kind(), "(" | ")" | ","))
        .collect()
}

/// A table constructor's entries, punctuation dropped.
pub(crate) fn field_nodes<'r>(table: &SgNode<'r>) -> Vec<SgNode<'r>> {
    table.children().filter(|c| c.kind() == "field").collect()
}

/// The name a table entry is keyed by, when a lexical path can spell it.
///
/// `alpha = f` and `['alpha'] = f` write the same key and answer the same
/// name. `[1] = f` and `[k] = f` answer `None`: the key is a value, not a
/// name — and the bracket is the only thing that says so, because the
/// grammar names `k` in `[k]` and `alpha` in `alpha = f` with the same field
/// and the same node kind.
pub(crate) fn field_name(field: &SgNode) -> Option<String> {
    let name = field.field("name")?;
    let bracketed = field.children().any(|c| c.kind() == "[");
    match (&*name.kind(), bracketed) {
        ("identifier", false) => Some(name.text().to_string()),
        ("string", true) => string_literal(&name),
        _ => None,
    }
}

/// The dotted path a variable or a declaration name spells, as written.
///
/// `M` → `["M"]`, `M.foo` → `["M", "foo"]`, `a.b.c` → `["a", "b", "c"]`, and
/// `M:bar` → `["M", "bar"]` — the colon form is sugar for the dotted one and
/// writes the very same table key, so normalising it here is what keeps one
/// slot from becoming two nodes. `None` when the leftmost thing is not a
/// name: `t[k].f` is a slot on a value.
fn name_path(node: &SgNode) -> Option<Vec<String>> {
    match &*node.kind() {
        "identifier" => Some(vec![node.text().to_string()]),
        // The grammar names the member `field` after a `.` and `method`
        // after a `:`; both write the same table key, so both are read the
        // same way.
        "dot_index_expression" => {
            let mut path = name_path(&node.field("table")?)?;
            path.push(node.field("field")?.text().to_string());
            Some(path)
        }
        "method_index_expression" => {
            let mut path = name_path(&node.field("table")?)?;
            path.push(node.field("method")?.text().to_string());
            Some(path)
        }
        _ => None,
    }
}

/// Whether this node sits inside a function body.
///
/// The chunk's own top level is the one place a `return` states the module's
/// exports; a `return` below a function states that function's result.
fn inside_a_function(node: &SgNode) -> bool {
    node.ancestors()
        .any(|a| matches!(&*a.kind(), "function_definition" | "function_declaration"))
}

/// The `(variable, value)` pairs one assignment writes, in source order.
///
/// `local a, b = f, g` writes two, and the grammar's `name`/`value` fields
/// answer only for the first — so the lists are paired here instead. A
/// shorter right-hand side simply supplies fewer pairs: `local a, b = f`
/// declares nothing for `b` that this file states.
fn assigned_pairs<'r>(assign: &SgNode<'r>) -> Vec<(SgNode<'r>, SgNode<'r>)> {
    let mut vars = Vec::new();
    let mut vals = Vec::new();
    for child in assign.children() {
        let target = match &*child.kind() {
            "variable_list" => &mut vars,
            "expression_list" => &mut vals,
            _ => continue,
        };
        target.extend(child.children().filter(|c| c.kind() != ","));
    }
    vars.into_iter().zip(vals).collect()
}

/// The owner path a *table entry* sits under, outermost first.
///
/// Walks out through every enclosing table constructor, composing the names
/// they were written under, and stops at the first thing that gives the
/// outermost table a name:
///
/// - an assignment — `M.a = { b = { c = f } }` gives `["M", "a", "b"]` for
///   `c`, and the pairing is positional so `local x, y = {..}, {..}` names
///   the right one;
/// - the chunk's own `return` — `return { exit = f }` gives `[]`, so the
///   entry is filed directly under the chunk, which is exactly what
///   `require 'm'.exit` names.
///
/// `None` when the outermost table has no name this file states: a table
/// literal passed as an argument, or returned from inside a function body.
fn table_owner(field: &SgNode) -> Option<Vec<String>> {
    let mut prefix: Vec<String> = Vec::new();
    let mut node = field.clone();
    loop {
        let table = node.parent()?;
        if table.kind() != "table_constructor" {
            return None;
        }
        let up = table.parent()?;
        match &*up.kind() {
            // `{ a = { b = f } }`: the enclosing entry names this table.
            "field" => {
                prefix.insert(0, field_name(&up)?);
                node = up;
            }
            "expression_list" => {
                let holder = up.parent()?;
                match &*holder.kind() {
                    "assignment_statement" => {
                        let (var, _) = assigned_pairs(&holder)
                            .into_iter()
                            .find(|(_, val)| val.range() == table.range())?;
                        let mut path = name_path(&var)?;
                        path.extend(prefix);
                        return Some(path);
                    }
                    // The chunk's export table. A `return` under a function
                    // states that function's result and names nothing.
                    "return_statement" if !inside_a_function(&holder) => return Some(prefix),
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
}

/// The path a function-valued declaration is written under, outermost first.
///
/// The four shapes this track reads, and nothing else:
/// `function f`, `function M.f`, `function M:f`, `M.f = function`, and a
/// function-valued entry of a named table.
fn declared_path(rule: &str, node: &SgNode) -> Option<Vec<String>> {
    match rule {
        "def-function" => name_path(&node.field("name")?),
        "assign" => None, // handled per pair; an assignment may write several
        "field" => {
            if node.field("value")?.kind() != "function_definition" {
                return None;
            }
            let mut path = table_owner(node)?;
            path.push(field_name(node)?);
            Some(path)
        }
        _ => None,
    }
}

/// The nearest *nameable* enclosing definition of a reference site.
///
/// A `require` at the top of a chunk belongs to nothing, and the driver
/// sources it at the file's own chunk node. One inside `function M.foo()`
/// belongs to `M.foo`. An anonymous function is stepped over rather than
/// stopped at — `it('x', function() require 'y' end)` sits in whatever names
/// *that*, and most often that is the chunk.
fn enclosing_definition(node: &SgNode) -> Option<Encloser> {
    for a in node.ancestors() {
        let path = match &*a.kind() {
            // `and_then` rather than `?`: a frame this file cannot name is a
            // frame to step over, not a reason to stop looking outward. An
            // error-tolerant parse can hand back a `function` with no name.
            "function_declaration" => a.field("name").and_then(|n| name_path(&n)),
            "function_definition" => value_path(&a),
            _ => continue,
        };
        if let Some(path) = path {
            return Some(Encloser {
                kind: kind_of(&path),
                path,
            });
        }
    }
    None
}

/// The path a function *expression* was written under, when one names it.
///
/// `M.f = function() end`, `local f = function() end`, and a function-valued
/// table entry. `None` for an anonymous one — an argument, a return value,
/// an operand.
fn value_path(function: &SgNode) -> Option<Vec<String>> {
    let up = function.parent()?;
    match &*up.kind() {
        "field" => {
            let mut path = table_owner(&up)?;
            path.push(field_name(&up)?);
            Some(path)
        }
        "expression_list" => {
            let assign = up.parent()?;
            if assign.kind() != "assignment_statement" {
                return None;
            }
            let (var, _) = assigned_pairs(&assign)
                .into_iter()
                .find(|(_, val)| val.range() == function.range())?;
            name_path(&var)
        }
        _ => None,
    }
}

/// A free function or a member: a path of one is the former, and anything
/// deeper names a table it was written on.
fn kind_of(path: &[String]) -> DefKind {
    if path.len() > 1 {
        DefKind::Method
    } else {
        DefKind::Function
    }
}

/// One definition, with the fields every Lua declaration shares.
fn def(kind: DefKind, path: Vec<String>, facets: DefFacets, span: Span) -> Option<Definition> {
    let (name, owner) = path.split_last()?;
    if name.is_empty() {
        return None;
    }
    Some(Definition {
        kind,
        name: name.clone(),
        owner: owner.to_vec(),
        space: DeclSpace::Value,
        facets,
        params: None,
        span,
    })
}

/// Extract one Lua file. The whole of the extractor's public surface.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<LuaLang> {
    static RULES: OnceLock<Rules> = OnceLock::new();
    let rules = RULES.get_or_init(|| Rules::compile(LUA_RULES).expect("lua.yml compiles"));

    let mut facts: FileFacts<LuaLang> = FileFacts {
        header: LuaHeader {
            rel_path: rel_path.to_string(),
            imports: Vec::new(),
        },
        defs: Vec::new(),
        refs: Vec::new(),
    };

    // The file's own chunk node, first, because the driver reads the first
    // `Module` definition as the file's container. Every `.lua` file is a
    // chunk whether or not it declares anything: a `require` naming an empty
    // file still resolves.
    let stem = rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path)
        .strip_suffix(".lua")
        .unwrap_or("");
    facts.defs.push(Definition {
        kind: DefKind::Module,
        name: stem.to_string(),
        owner: Vec::new(),
        space: DeclSpace::Namespace,
        facets: DefFacets::SYNTHETIC,
        params: None,
        span: Span {
            byte_start: 0,
            byte_end: source.len() as u32,
            line: 1,
        },
    });

    let tree = SourceTree::parse_lua(source);
    for (rule, node) in tree.matches(rules) {
        match rule {
            "def-function" | "field" => {
                if let Some(path) = declared_path(rule, &node)
                    && let Some(d) = def(kind_of(&path), path, DefFacets::default(), span_of(&node))
                {
                    facts.defs.push(d);
                }
            }
            "assign" => assignment(&mut facts, &node),
            "call" => call(&mut facts, &node),
            _ => {}
        }
    }
    // Rules run one at a time, so the records arrive rule-major; source order
    // is what a reader of a report expects and what a span-keyed pairing
    // needs to be stable under.
    facts.defs[1..].sort_by_key(|d| d.span.byte_start);
    facts.refs.sort_by_key(|r| r.span.byte_start);
    facts.header.imports.sort_by_key(|i| i.span.byte_start);
    facts
}

/// `M.f = function() end`, `local f = function() end`, and the multi-target
/// forms of both. Only a function-valued target declares anything here.
fn assignment(facts: &mut FileFacts<LuaLang>, node: &SgNode) {
    for (var, val) in assigned_pairs(node) {
        if val.kind() != "function_definition" {
            continue;
        }
        let Some(path) = name_path(&var) else {
            continue;
        };
        if let Some(d) = def(kind_of(&path), path, DefFacets::default(), span_of(&val)) {
            facts.defs.push(d);
        }
    }
}

/// `require` reached from a call site, in either of the two shapes this
/// extractor reads. Every other call contributes nothing: tier 2 emits no
/// call reference.
fn call(facts: &mut FileFacts<LuaLang>, node: &SgNode) {
    let Some(callee) = node.field("name") else {
        return;
    };
    if callee.kind() != "identifier" {
        return; // `busted.subscribe(...)`, `s:set(...)`: not an import
    }
    let args = arg_nodes(node);
    match &*callee.text() {
        "require" => import(facts, node, args.first()),
        // `pcall(require, 'moonscript')`: the callee is `pcall` and the
        // module `require` would load is its second argument.
        "pcall" if args.first().is_some_and(|a| a.text() == "require") => {
            import(facts, node, args.get(1))
        }
        _ => {}
    }
}

/// One import site and its reference.
///
/// `specifier` is the argument that names the module. `None` — `require()`
/// with no argument at all — is not an import site and produces nothing:
/// there is no module named, so there is nothing to fail to resolve.
fn import(facts: &mut FileFacts<LuaLang>, call: &SgNode, specifier: Option<&SgNode>) {
    let Some(specifier) = specifier else { return };
    let span = span_of(call);
    let literal = string_literal(specifier);
    let form = match &literal {
        Some(name) => ImportForm::Module(name.clone()),
        None => ImportForm::Dynamic,
    };
    let target = match &literal {
        Some(name) => RefTarget {
            root: TargetRoot::Name,
            segments: vec![name.clone()],
        },
        // The root is not a name: a computed specifier is exactly the shape
        // `TargetRoot::Expr` exists for.
        None => RefTarget {
            root: TargetRoot::Expr,
            segments: Vec::new(),
        },
    };
    facts.header.imports.push(ImportSpec { form, span });
    facts.refs.push(Reference {
        kind: RefKind::Import,
        space: DeclSpace::Namespace,
        // The site as written, which is what a `RefKey` is keyed on and what
        // a query prints back.
        raw_target: call.text().split_whitespace().collect::<Vec<_>>().join(" "),
        target,
        // Tier 2 emits no expression-level reference, so nothing here can
        // name a local: `LocalBinding` does not apply to this track.
        locally_bound: false,
        argc: None,
        arg_types: None,
        enclosing: enclosing_definition(call),
        span,
    });
}

/// The Lua extractor, as the driver holds it.
pub struct LuaExtractor;

impl Extractor<LuaLang> for LuaExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<LuaLang> {
        extract(rel_path, source)
    }
}
