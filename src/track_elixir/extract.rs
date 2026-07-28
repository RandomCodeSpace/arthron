//! Elixir extractor: one file in, records out. Forbidden from linking.
//!
//! The YAML rule (embedded from `rules/elixir.yml`) selects **every call**,
//! because Elixir has no declaration node: `defmodule`, `def`, `alias` and
//! the rest are ordinary calls whose `target` is the identifier that spells
//! them. This module reads that identifier and decides what the call is.
//!
//! # What a tier-2, best-effort extractor emits, and what it must not
//!
//! Definitions and structure, plus the **four import-like directives and
//! nothing else**: `alias`, `import`, `require` and `use`, each of which
//! names a module. Elixir's gate is an import-resolution rate, so a call
//! site, an `@behaviour` or a `Plug.Conn.t()` in a typespec emitted here
//! would enter a denominator nothing in this track can resolve — tier-1
//! coverage claimed without tier-1 measurement.
//!
//! `use` earns its place beside the other three: it names a module, and the
//! module must exist and be compiled for the site to compile. That the
//! expansion then injects code is a fact about `__using__/1`, not about the
//! reference, and modelling the injection is not this layer's business.
//!
//! # Two compositions the source never writes
//!
//! - **A nested `defmodule`.** `defmodule InvalidCSRFTokenError` inside
//!   `defmodule Plug.CSRFProtection` declares
//!   `Plug.CSRFProtection.InvalidCSRFTokenError`, and that string appears
//!   nowhere in the file. 64 of the corpus's 154 modules are written this
//!   way, and a reference elsewhere names the composed form.
//! - **The alias environment.** `alias Plug.Conn` binds `Conn`, so a later
//!   `import Conn` names `Plug.Conn`. Both are file-local facts — no other
//!   file participates — so composing them here is name composition and not
//!   linking, exactly as composing a nested module's name is. Getting it
//!   wrong would file an in-repository module as somebody else's, which is
//!   the one mistake [`crate::track_elixir::resolve`]'s `External` rule
//!   cannot survive.
//!
//! Bindings come from three places, which is all Elixir has: `alias A.B`
//! binds `B`, `alias A.B, as: C` and `require A.B, as: C` bind `C`, and a
//! nested `defmodule` binds its own last segment inside the module that
//! holds it. Each reaches forward only, and only inside the module body that
//! wrote it — checked by byte range, so two modules in one file (26 files in
//! the corpus do this) never see each other's.
//!
//! # Recorded under-counts
//!
//! Each is a known shortfall, written down rather than left to be
//! rediscovered, and none may be closed by widening a bucket:
//!
//! - **Anything a `quote` block declares.** `use Plug.Builder` expands a
//!   `__using__/1` whose body is a `quote` full of `def`s; those functions
//!   are declared in the *using* module, which this file does not name. No
//!   node is invented for them. A directive inside the same block still
//!   emits its reference — the module it names is the same module wherever
//!   the expansion lands — sourced at the macro that holds it.
//! - **Module attributes.** `@type t :: ...`, `@callback init/1` and
//!   `@version "1.0"` are one syntactic form, and telling a declaration from
//!   an annotation from a compile-time constant needs a list of attribute
//!   names written from memory. This track writes no such list, so it reads
//!   no attribute at all.
//! - **Default arguments.** `def f(a, b \\ 1)` defines `f/1` and `f/2`;
//!   only the written arity becomes a node.
//! - **What `defstruct` and `defexception` generate besides their keys** —
//!   `__struct__/0`, `__struct__/1`, `exception/1`, `message/1`. The keys
//!   are written and are emitted; the functions are not written and are not.
//! - **`defimpl` without a `for:`, and with a list of them.** The first
//!   defaults to the enclosing module and the second declares one module per
//!   element; the corpus writes neither, and a name composed blind is worse
//!   than a name not composed.
//! - **`defdelegate`'s `to:`.** It names a module, and it names it as the
//!   target of a *call* this tier does not emit. The delegating function is
//!   a definition and is emitted; the delegation is not a reference here.
//! - **A declaration whose name is computed.** `def unquote(name)(x)` names
//!   something only the expansion knows.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, Params, RefKind, RefTarget, Reference,
    Span, TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_elixir::lang::{ElixirLang, function_key};

/// The embedded Elixir extraction rules.
const ELIXIR_RULES: &str = include_str!("../rules/elixir.yml");

/// Which of the four directives a clause is.
///
/// They differ in what they do at compile time and in whether they bind a
/// name, and in nothing else this track reads: all four name one module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Directive {
    /// `alias A.B` — binds `B`, or the `as:` name.
    Alias,
    /// `import A.B` — makes the module's functions callable unqualified.
    Import,
    /// `require A.B` — makes its macros available; binds only with `as:`.
    Require,
    /// `use A.B` — requires the module and invokes its `__using__/1`.
    Use,
}

impl Directive {
    /// The keyword, as written.
    pub fn name(self) -> &'static str {
        match self {
            Directive::Alias => "alias",
            Directive::Import => "import",
            Directive::Require => "require",
            Directive::Use => "use",
        }
    }

    /// The directive a call's target identifier spells, if it is one.
    fn of(word: &str) -> Option<Directive> {
        Some(match word {
            "alias" => Directive::Alias,
            "import" => Directive::Import,
            "require" => Directive::Require,
            "use" => Directive::Use,
            _ => return None,
        })
    }
}

/// What a directive names, as far as one file can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportForm {
    /// A module name, in segments, with any file-local alias expanded.
    Module(Vec<String>),
    /// The target is not a literal module name: `require unquote(target)`,
    /// `alias __MODULE__.Sub`, `alias :"Elixir.Foo"`. Never guessed.
    Dynamic,
}

impl ImportForm {
    /// The form's name, for a census that has to be readable.
    pub fn name(&self) -> &'static str {
        match self {
            ImportForm::Module(_) => "module",
            ImportForm::Dynamic => "dynamic",
        }
    }
}

/// One directive clause: which one it is, what it names, and where it sits.
///
/// Every `ImportSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is how the
/// resolver pairs the two without the core learning what an `alias` is. The
/// span is the *target expression's*, not the whole call's, so that
/// `alias Plug.{Conn, Router}` — two references from one clause — keys two
/// distinct entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    /// Which directive wrote it.
    pub directive: Directive,
    /// What it names.
    pub form: ImportForm,
    /// Where the named target sits.
    pub span: Span,
}

/// Per-file Elixir facts only the Elixir resolver reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElixirHeader {
    /// Repo-relative, `/`-separated path of the file. Diagnostics only: no
    /// Elixir identity is derived from a path, because no Elixir reference
    /// names one.
    pub rel_path: String,
    /// Every directive clause, in source order.
    pub imports: Vec<ImportSpec>,
}

/// Call targets that declare a module.
const MODULE_FORMS: &[&str] = &["defmodule", "defprotocol"];

/// Call targets that declare a function or a macro, with what they say about
/// it: `(form, exported, present at runtime, always carries a body)`.
///
/// A macro is expanded at compile time and is gone when the program runs,
/// which is what [`DefFacets::RUNTIME`]'s absence records. `defguard` and
/// `defdelegate` carry their body in an option or a guard rather than in a
/// block, so the "declared without a body" test does not apply to them.
const FUNCTION_FORMS: &[(&str, bool, bool, bool)] = &[
    ("def", true, true, false),
    ("defp", false, true, false),
    ("defmacro", true, false, false),
    ("defmacrop", false, false, false),
    ("defguard", true, false, true),
    ("defguardp", false, false, true),
    ("defdelegate", true, true, true),
];

/// Call targets that declare a struct's keys.
const STRUCT_FORMS: &[&str] = &["defstruct", "defexception"];

/// Punctuation and trivia the grammar exposes as children.
fn is_trivia(kind: &str) -> bool {
    matches!(
        kind,
        "(" | ")" | "," | "[" | "]" | "{" | "}" | "do" | "end" | "." | "@" | ":" | "\"" | "comment"
    )
}

/// A node's children with punctuation and comments dropped.
fn significant<'r>(node: &SgNode<'r>) -> Vec<SgNode<'r>> {
    node.children().filter(|c| !is_trivia(&c.kind())).collect()
}

/// The first child of a given kind.
fn child_of<'r>(node: &SgNode<'r>, kind: &str) -> Option<SgNode<'r>> {
    node.children().find(|c| c.kind() == kind)
}

/// The identifier a call names, when it names one at all.
///
/// `None` for `unquote(name)(x)`, whose target is itself a call.
fn call_target(node: &SgNode) -> Option<String> {
    let target = node.field("target")?;
    (target.kind() == "identifier").then(|| target.text().to_string())
}

/// A dotted module name, split into segments.
fn segments(dotted: &str) -> Vec<String> {
    dotted.split('.').map(str::to_string).collect()
}

/// Source text with every run of whitespace collapsed, so a clause written
/// across lines still keys one readable row.
fn one_line(node: &SgNode) -> String {
    node.text().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One module the file declares, as everything below it needs to see it.
#[derive(Debug, Clone)]
struct ModuleFrame {
    /// The composed name — what a reference in another file spells.
    name: String,
    /// The call's byte range, which is the lexical extent of its body.
    range: (u32, u32),
}

/// One name bound to a module, and where the binding reaches.
#[derive(Debug, Clone)]
struct Binding {
    /// The bound name — `Conn`, or the `as:` name.
    name: String,
    /// What it stands for, fully expanded.
    module: Vec<String>,
    /// The byte range of the module body that wrote it, or the whole file.
    scope: (u32, u32),
    /// Where it starts reaching: bindings work forward only.
    at: u32,
}

/// The lexical context of one call: which module holds it, which function
/// holds it, and whether a `quote` block stands between.
struct Context {
    /// The nearest enclosing module *outside* any `quote`.
    module: Option<ModuleFrame>,
    /// The nearest enclosing function's `name/arity` key, outside any
    /// `quote`. `None` at module level, and for a function whose own name is
    /// computed.
    function: Option<String>,
    /// A `quote` block stands between this node and its enclosing
    /// definition, so what it declares is declared somewhere else.
    quoted: bool,
}

impl Context {
    /// The scope a binding written here reaches: the enclosing module's body,
    /// or the whole file.
    fn scope(&self, source_len: u32) -> (u32, u32) {
        self.module.as_ref().map_or((0, source_len), |m| m.range)
    }

    /// The nearest nameable enclosing definition of a site here.
    fn encloser(&self) -> Option<Encloser> {
        let module = self.module.as_ref()?;
        Some(match &self.function {
            Some(key) => Encloser {
                path: vec![module.name.clone(), key.clone()],
                kind: DefKind::Function,
            },
            None => Encloser {
                path: vec![module.name.clone()],
                kind: DefKind::Module,
            },
        })
    }
}

/// Everything one pass over the file accumulates.
struct Pass {
    facts: FileFacts<ElixirLang>,
    /// Composed module names and bodies, by the declaring call's start.
    modules: HashMap<u32, ModuleFrame>,
    /// Function keys, by the declaring call's start.
    functions: HashMap<u32, String>,
    /// Every binding, in the order the file wrote them.
    bindings: Vec<Binding>,
    /// The file's length, which is the scope of a file-level binding.
    source_len: u32,
}

impl Pass {
    /// The lexical context of one call.
    ///
    /// Walks outward. A `quote` discards everything collected so far —
    /// those frames were inside it — so what comes back is the innermost
    /// definition that really holds this site.
    fn context(&self, node: &SgNode) -> Context {
        let mut ctx = Context {
            module: None,
            function: None,
            quoted: false,
        };
        for a in node.ancestors() {
            if a.kind() != "call" {
                continue;
            }
            let Some(word) = call_target(&a) else {
                continue;
            };
            let at = a.range().start as u32;
            if word == "quote" {
                ctx.quoted = true;
                ctx.module = None;
                ctx.function = None;
            } else if FUNCTION_FORMS.iter().any(|(f, ..)| *f == word) {
                if ctx.function.is_none() && ctx.module.is_none() {
                    ctx.function = self.functions.get(&at).cloned();
                }
            } else if (MODULE_FORMS.contains(&word.as_str()) || word == "defimpl")
                && ctx.module.is_none()
            {
                ctx.module = self.modules.get(&at).cloned();
            }
        }
        ctx
    }

    /// A module name with its head replaced by whatever this file binds it
    /// to, and whether any binding applied — a name that expanded is
    /// absolute and no longer nests.
    fn expand(&self, at: u32, path: &[String]) -> (Vec<String>, bool) {
        let Some(head) = path.first() else {
            return (path.to_vec(), false);
        };
        for b in self.bindings.iter().rev() {
            if b.at < at && b.scope.0 <= at && at < b.scope.1 && b.name == *head {
                let mut out = b.module.clone();
                out.extend_from_slice(&path[1..]);
                return (out, true);
            }
        }
        (path.to_vec(), false)
    }

    /// Record one definition.
    fn declare(&mut self, def: Definition) {
        self.facts.defs.push(def);
    }
}

/// Extract one Elixir file. The whole of the extractor's public surface.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<ElixirLang> {
    static RULES: OnceLock<Rules> = OnceLock::new();
    let rules = RULES.get_or_init(|| Rules::compile(ELIXIR_RULES).expect("elixir.yml compiles"));

    let mut pass = Pass {
        facts: FileFacts {
            header: ElixirHeader {
                rel_path: rel_path.to_string(),
                imports: Vec::new(),
            },
            defs: Vec::new(),
            refs: Vec::new(),
        },
        modules: HashMap::new(),
        functions: HashMap::new(),
        bindings: Vec::new(),
        source_len: source.len() as u32,
    };

    let tree = SourceTree::parse_elixir(source);
    let mut calls: Vec<SgNode> = tree.matches(rules).into_iter().map(|(_, n)| n).collect();
    // Source order, which is also outermost-first: a call that encloses
    // another starts before it. Every step below reads facts an enclosing
    // call already recorded — a module's composed name, a binding's reach —
    // so one pass in this order is enough and no fixed point is needed.
    calls.sort_by_key(|n| n.range().start);

    for node in &calls {
        // A call sitting in another call's argument list is an argument, not
        // a statement: the `alias(x)` in `def alias(x)` is a parameter list
        // and not a directive.
        if node
            .ancestors()
            .next()
            .is_some_and(|p| p.kind() == "arguments")
        {
            continue;
        }
        let Some(word) = call_target(node) else {
            continue;
        };
        if MODULE_FORMS.contains(&word.as_str()) {
            module_declaration(&mut pass, node);
        } else if word == "defimpl" {
            impl_declaration(&mut pass, node);
        } else if let Some(form) = FUNCTION_FORMS.iter().find(|(f, ..)| *f == word) {
            function_declaration(&mut pass, node, form);
        } else if STRUCT_FORMS.contains(&word.as_str()) {
            struct_declaration(&mut pass, node);
        } else if let Some(directive) = Directive::of(&word) {
            directive_clause(&mut pass, node, directive);
        }
    }
    pass.facts
}

/// `defmodule A.B do … end` and `defprotocol A.B do … end`.
fn module_declaration(pass: &mut Pass, node: &SgNode) {
    let ctx = pass.context(node);
    if ctx.quoted {
        // A `defmodule` inside a `quote` declares a module when the macro
        // expands, under a name the expansion may build. Nothing here is a
        // fact about this file, so nothing is recorded — not the definition,
        // not the frame the body would hang on, and not the alias a nested
        // module creates.
        return;
    }
    let at = node.range().start as u32;
    let Some(args) = child_of(node, "arguments") else {
        return;
    };
    let Some(written) = significant(&args).first().cloned() else {
        return;
    };
    if written.kind() != "alias" {
        return; // a computed module name: nothing here can say what it is
    }
    let (path, aliased) = pass.expand(at, &segments(&written.text()));
    // `Elixir.` is the root every module name already carries, so writing it
    // escapes the enclosing module instead of nesting under it — and so does
    // a head some `alias` already bound to an absolute name.
    let (path, absolute) = match path.split_first() {
        Some((head, rest)) if head == "Elixir" && !rest.is_empty() => (rest.to_vec(), true),
        _ => (path, aliased),
    };
    let name = path.join(".");
    if name.is_empty() {
        return;
    }
    let owner: Vec<String> = match (&ctx.module, absolute) {
        (Some(m), false) => vec![m.name.clone()],
        _ => Vec::new(),
    };
    let composed = crate::track_elixir::lang::module_fqn(&owner, &name);
    let range = (at, node.range().end as u32);
    pass.modules.insert(
        at,
        ModuleFrame {
            name: composed.clone(),
            range,
        },
    );
    // A module nested inside another is automatically aliased by its last
    // segment inside the module that holds it. Elixir's own rule, and the
    // reason `defimpl Plug.Exception, for: ActionableError` inside
    // `defmodule Plug.DebuggerTest` names the nested error and not a
    // top-level one.
    if let (Some(outer), Some(last)) = (&ctx.module, path.last()) {
        pass.bindings.push(Binding {
            name: last.clone(),
            module: segments(&composed),
            scope: outer.range,
            at,
        });
    }
    pass.declare(module_definition(
        name,
        owner,
        DefFacets::default(),
        span_of(node),
    ));
}

/// `defimpl Protocol, for: Type do … end`.
///
/// The module it declares is `Protocol.Type` — `Module.concat/2` of the two,
/// which is absolute: the module the `defimpl` is *written* inside
/// contributes nothing to the name.
fn impl_declaration(pass: &mut Pass, node: &SgNode) {
    if pass.context(node).quoted {
        return; // declared by the expansion, for the reason above
    }
    let at = node.range().start as u32;
    let Some(args) = child_of(node, "arguments") else {
        return;
    };
    let items = significant(&args);
    let Some(protocol) = items.first() else {
        return;
    };
    if protocol.kind() != "alias" {
        return;
    }
    let Some(target) = keyword_value(&items, "for") else {
        return; // defaults to the enclosing module; not composed blind
    };
    if target.kind() != "alias" {
        return;
    }
    let (protocol_path, _) = pass.expand(at, &segments(&protocol.text()));
    let (target_path, _) = pass.expand(at, &segments(&target.text()));
    let mut path = protocol_path;
    path.extend(target_path);
    let name = path.join(".");
    if name.is_empty() {
        return;
    }
    pass.modules.insert(
        at,
        ModuleFrame {
            name: name.clone(),
            range: (at, node.range().end as u32),
        },
    );
    pass.declare(module_definition(
        name,
        Vec::new(),
        DefFacets::SYNTHETIC,
        span_of(node),
    ));
}

/// One module declaration, however it was spelled.
fn module_definition(
    name: String,
    owner: Vec<String>,
    facets: DefFacets,
    span: Span,
) -> Definition {
    Definition {
        kind: DefKind::Module,
        name,
        owner,
        space: DeclSpace::Namespace,
        facets,
        params: None,
        span,
    }
}

/// `def`, `defp`, `defmacro`, `defmacrop`, `defguard`, `defguardp`,
/// `defdelegate`.
fn function_declaration(pass: &mut Pass, node: &SgNode, form: &(&str, bool, bool, bool)) {
    let (_, exported, runtime, always_has_body) = *form;
    let ctx = pass.context(node);
    let at = node.range().start as u32;
    let Some(args) = child_of(node, "arguments") else {
        return;
    };
    let Some(head) = significant(&args).first().cloned() else {
        return;
    };
    let Some((name, arity)) = signature(&head) else {
        return; // `def unquote(name)(x)`: only the expansion knows
    };
    // The key is recorded whether or not the definition is: a reference
    // inside a quoted body is still sourced at the macro that holds it.
    pass.functions.insert(at, function_key(&name, arity));
    if ctx.quoted {
        return; // declared in whatever module `use`s this one
    }
    let Some(module) = ctx.module else {
        return; // Elixir has no function outside a module
    };
    let mut facets = DefFacets::default();
    if exported {
        facets = facets.union(DefFacets::EXPORTED);
    }
    if runtime {
        facets = facets.union(DefFacets::RUNTIME);
    }
    if !always_has_body && !has_body(node) {
        facets = facets.union(DefFacets::ABSTRACT);
    }
    pass.declare(Definition {
        kind: DefKind::Function,
        name,
        owner: vec![module.name],
        space: DeclSpace::Value,
        facets,
        params: Some(Params {
            count: arity,
            varargs: false,
            types: Vec::new(),
        }),
        span: span_of(node),
    });
}

/// `defstruct` and `defexception`: one field per declared key.
///
/// The struct itself is the module, which is already a node; these are the
/// keys written beside it, exactly as Ruby's `attr_reader` symbols are.
fn struct_declaration(pass: &mut Pass, node: &SgNode) {
    let ctx = pass.context(node);
    if ctx.quoted {
        return;
    }
    let Some(module) = ctx.module else {
        return;
    };
    let Some(args) = child_of(node, "arguments") else {
        return;
    };
    let mut keys: Vec<(String, Span)> = Vec::new();
    for item in significant(&args) {
        collect_keys(&item, &mut keys);
    }
    for (name, span) in keys {
        pass.declare(Definition {
            kind: DefKind::Field,
            name,
            owner: vec![module.name.clone()],
            space: DeclSpace::Value,
            facets: DefFacets::SYNTHETIC,
            params: None,
            span,
        });
    }
}

/// The keys one `defstruct`/`defexception` argument declares.
///
/// Three shapes, all of which the corpus writes: a list of atoms, a keyword
/// list of defaults, and a list mixing the two. Anything else — `defstruct
/// @fields` — declares keys this file does not state, and states none.
fn collect_keys(node: &SgNode, out: &mut Vec<(String, Span)>) {
    match &*node.kind() {
        "list" | "keywords" => {
            for child in significant(node) {
                collect_keys(&child, out);
            }
        }
        "atom" => {
            let text = node.text();
            if let Some(name) = text.strip_prefix(':')
                && !name.is_empty()
            {
                out.push((name.to_string(), span_of(node)));
            }
        }
        "pair" => {
            if let Some(key) = node.children().next()
                && key.kind() == "keyword"
            {
                let text = key.text();
                let name = text.trim().trim_end_matches(':');
                if !name.is_empty() {
                    out.push((name.to_string(), span_of(node)));
                }
            }
        }
        _ => {}
    }
}

/// `alias`, `import`, `require`, `use`: one reference per module named.
fn directive_clause(pass: &mut Pass, node: &SgNode, directive: Directive) {
    let ctx = pass.context(node);
    let at = node.range().start as u32;
    let Some(args) = child_of(node, "arguments") else {
        return; // `alias` with no argument is not a directive site
    };
    let items = significant(&args);
    let Some(first) = items.first() else {
        return;
    };
    let renamed = keyword_value(&items, "as")
        .filter(|v| v.kind() == "alias")
        .map(|v| v.text().to_string());

    // What the clause names, one entry per module, each with the text the
    // site spells for it. A tuple clause distributes its prefix, which is
    // what the source says even though it writes the prefix once.
    let named: Vec<(SgNode, Vec<String>, String)> = match &*first.kind() {
        "alias" => vec![(
            first.clone(),
            segments(&first.text()),
            first.text().to_string(),
        )],
        "dot" => multi_alias(first),
        _ => Vec::new(),
    };

    if named.is_empty() {
        let span = span_of(first);
        emit(
            pass,
            directive,
            ImportForm::Dynamic,
            format!("{} {}", directive.name(), one_line(first)),
            RefTarget {
                root: TargetRoot::Expr,
                segments: Vec::new(),
            },
            span,
            &ctx,
        );
        return;
    }

    for (site, path, written) in named {
        let (path, _) = pass.expand(at, &path);
        emit(
            pass,
            directive,
            ImportForm::Module(path.clone()),
            format!("{} {written}", directive.name()),
            RefTarget {
                root: TargetRoot::Name,
                segments: path.clone(),
            },
            span_of(&site),
            &ctx,
        );
        // A binding reaches forward, and only inside the module body that
        // wrote it. One written inside a function body binds nothing outside
        // it, and one inside a `quote` binds in the expansion rather than
        // here — so neither is recorded, which costs a resolution rather
        // than inventing one.
        if ctx.quoted || ctx.function.is_some() {
            continue;
        }
        let bound = match (directive, &renamed) {
            (Directive::Alias, Some(as_name)) => Some(as_name.clone()),
            (Directive::Alias, None) => path.last().cloned(),
            (Directive::Require, Some(as_name)) => Some(as_name.clone()),
            _ => None,
        };
        if let Some(name) = bound {
            let scope = ctx.scope(pass.source_len);
            pass.bindings.push(Binding {
                name,
                module: path,
                scope,
                at,
            });
        }
    }
}

/// One clause, one reference, and the spec the resolver pairs with it.
fn emit(
    pass: &mut Pass,
    directive: Directive,
    form: ImportForm,
    raw_target: String,
    target: RefTarget,
    span: Span,
    ctx: &Context,
) {
    pass.facts.header.imports.push(ImportSpec {
        directive,
        form,
        span,
    });
    pass.facts.refs.push(Reference {
        kind: RefKind::Import,
        space: DeclSpace::Namespace,
        raw_target,
        target,
        // Tier 2 emits no expression-level reference, so nothing here can
        // name a local: `LocalBinding` does not apply to this track.
        locally_bound: false,
        argc: None,
        enclosing: ctx.encloser(),
        span,
    });
}

/// `alias Plug.{Conn, Router}`: the prefix, distributed over the members.
fn multi_alias<'r>(dot: &SgNode<'r>) -> Vec<(SgNode<'r>, Vec<String>, String)> {
    let (Some(left), Some(right)) = (dot.field("left"), dot.field("right")) else {
        return Vec::new();
    };
    if left.kind() != "alias" || right.kind() != "tuple" {
        return Vec::new(); // `__MODULE__.Sub`: the head is not a module name
    }
    let prefix = segments(&left.text());
    significant(&right)
        .into_iter()
        .filter(|m| m.kind() == "alias")
        .map(|member| {
            let mut path = prefix.clone();
            path.extend(segments(&member.text()));
            let written = path.join(".");
            (member, path, written)
        })
        .collect()
}

/// The value of one keyword option in a clause's arguments.
fn keyword_value<'r>(items: &[SgNode<'r>], want: &str) -> Option<SgNode<'r>> {
    for item in items {
        if item.kind() != "keywords" {
            continue;
        }
        for pair in item.children().filter(|c| c.kind() == "pair") {
            let mut children = pair.children();
            let Some(key) = children.next() else { continue };
            if key.kind() != "keyword" {
                continue;
            }
            let text = key.text();
            if text.trim().trim_end_matches(':') == want {
                return pair.field("value").or_else(|| children.next());
            }
        }
    }
    None
}

/// A declaration head's `(name, arity)`.
fn signature(head: &SgNode) -> Option<(String, u32)> {
    match &*head.kind() {
        // `def init, do: []` — a name with no parameter list at all.
        "identifier" => Some((head.text().to_string(), 0)),
        "call" => {
            let name = call_target(head)?;
            let arity = child_of(head, "arguments")
                .map(|a| significant(&a).len() as u32)
                .unwrap_or(0);
            Some((name, arity))
        }
        // `def f(x) when is_atom(x)` — the guard is not part of the head.
        "binary_operator" => {
            let operator = head.field("operator")?;
            (operator.kind() == "when").then(|| signature(&head.field("left")?))?
        }
        _ => None,
    }
}

/// Whether a declaration carries a body: a `do` block, or a `do:` option.
fn has_body(call: &SgNode) -> bool {
    if child_of(call, "do_block").is_some() {
        return true;
    }
    let Some(args) = child_of(call, "arguments") else {
        return false;
    };
    keyword_value(&significant(&args), "do").is_some()
}

/// The Elixir extractor, as the driver holds it.
pub struct ElixirExtractor;

impl Extractor<ElixirLang> for ElixirExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<ElixirLang> {
        extract(rel_path, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_compile() {
        Rules::compile(ELIXIR_RULES).expect("elixir.yml compiles");
    }

    #[test]
    fn a_broken_file_yields_records_rather_than_a_panic() {
        // tree-sitter is error-tolerant, and a file that does not parse is
        // still a file this scan read.
        let facts = extract("lib/broken.ex", "defmodule ((( do\n  alias\n");
        assert!(facts.refs.len() <= 1, "{:?}", facts.refs);
    }

    #[test]
    fn a_directive_that_is_a_parameter_list_is_not_a_directive() {
        // `def alias(x)` writes a call whose target is `alias`; it sits in
        // another call's arguments, which is what tells the two apart.
        let facts = extract("lib/app.ex", "defmodule A do\n  def alias(x), do: x\nend\n");
        assert!(facts.refs.is_empty(), "{:?}", facts.refs);
        assert_eq!(facts.defs[1].name, "alias");
    }
}
