//! Dart extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/dart.yml`) select nodes by kind; this
//! module interprets their children.
//!
//! # What a best-effort tier-2 extractor emits, and what it must not
//!
//! Definitions and structure, plus **the URIs the library directives name and
//! nothing else**. Dart's gate is an import-resolution rate, so a call site or
//! an `implements Iterable<E>` emitted here would enter a denominator nothing
//! in this track resolves — tier-1 coverage claimed without tier-1
//! measurement. `class C extends B` is therefore read as part of `C`'s
//! structure and produces no [`RefKind::Inherit`] reference, and no call site
//! produces a [`RefKind::Call`] one.
//!
//! Four directives name a URI and all four are emitted, because all four name
//! a file the same way: `import`, `export`, `part` and `part of`. An `export`
//! is a [`RefKind::Export`] and the other three are [`RefKind::Import`] — the
//! same reference with a different verb, which is what lets a file both import
//! and export one library without the two collapsing into one row.
//!
//! **One reference per URI, not per directive.** A configurable import —
//! `import 'x.dart' if (dart.library.io) 'y.dart'` — names two libraries and
//! this build cannot know which configuration a reader compiles under, so both
//! are references and both are resolved. Choosing the default would drop the
//! other; choosing neither would drop both.
//!
//! # Recorded under-counts
//!
//! Each is a known shortfall, written down rather than left to be
//! rediscovered, and none may be closed by widening a bucket:
//!
//! - **`show` and `hide` combinators.** `import 'a.dart' show A, B` names two
//!   *declarations* in the library it imports, not two libraries. Resolving
//!   one means computing that library's exported name set, which recurses
//!   through every barrel it re-exports — the tier-1 export-map problem the
//!   EcmaScript track solves and this one does not. The names are recorded on
//!   [`UriSpec::combinators`] so the census can see them, and no reference is
//!   emitted for them: a name put into a denominator and answered by guessing
//!   is worth less than a name honestly not counted.
//! - **`part of <dotted.name>`.** The legacy spelling names a library by the
//!   name its `library` directive declares rather than by a URI. This track
//!   indexes libraries by path, so nothing here could resolve it and no
//!   reference is emitted; the URI spelling — the only one modern Dart writes
//!   — is emitted and resolved.
//! - **A part file's declarations belong to its part, not to its library.**
//!   Dart says a `part` file's declarations are the *enclosing* library's.
//!   This track roots every FQN at the file that wrote the declaration, so a
//!   repository using `part` gets one library node per file and members under
//!   the wrong one. The measured corpus writes no `part` at all, which is why
//!   the simpler model is the one that shipped.
//! - **A declaration inside a body.** A local function or a class member
//!   declared inside a closure is a real declaration Dart scopes to that body.
//!   Locals are not nodes by decision, so nothing is emitted for one.
//! - **An unnamed `extension`.** `extension on Iterable<E> { … }` declares
//!   members reachable only through the type it extends; there is no name a
//!   reference could use, so no node is invented for it or for its members.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, RefKind, RefTarget, Reference, Span, TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_dart::lang::DartLang;

/// The embedded Dart extraction rules.
const DART_RULES: &str = include_str!("../rules/dart.yml");

/// Declaration modifiers this extractor reads, as the grammar spells them.
const MODIFIERS: &[&str] = &[
    "static", "const", "final", "late", "abstract", "external", "factory",
];

/// Node kinds that stand between a declaration and its modifiers, and may be
/// walked through while looking for one.
const WRAPPERS: &[&str] = &[
    "method_signature",
    "method_declaration",
    "declaration",
    "class_member",
    "top_level_variable_declaration",
    "initialized_identifier_list",
    "static_final_declaration_list",
    "function_declaration",
    "getter_declaration",
    "setter_declaration",
];

/// Node kinds whose inside is a body or an expression, where a declaration is
/// a local rather than a member.
const BODIES: &[&str] = &[
    "block",
    "function_body",
    "function_expression_body",
    "function_expression",
    "lambda_expression",
    "formal_parameter_list",
    "arguments",
    "initializers",
    "local_variable_declaration",
    "local_function_declaration",
];

/// How a directive spells the library it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UriForm {
    /// A string literal, unquoted and concatenated: `'src/wrappers.dart'`.
    Literal(String),
    /// The literal could not be read as one — an interpolation, which Dart's
    /// own grammar forbids in a URI and tree-sitter still parses. Never
    /// guessed.
    Dynamic,
}

/// One URI a directive names: what it spells, what came with it, and where it
/// sits.
///
/// Every `UriSpec` shares its [`Span`] with exactly one reference in the same
/// [`FileFacts`], which is how the resolver pairs the two without the core
/// learning what an `import` is. The span is the **URI node's**, not the
/// directive's, so a directive naming two URIs still pairs one to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriSpec {
    /// What the URI spells.
    pub form: UriForm,
    /// The directive's verb, as written: `import`, `export`, `part`,
    /// `part of`.
    pub directive: &'static str,
    /// The names a `show`/`hide` combinator lists, in source order.
    /// Structure, never a reference — see the module header.
    pub combinators: Vec<String>,
    /// Where the URI sits.
    pub span: Span,
}

/// Per-file Dart facts only the Dart resolver reads.
///
/// `rel_path` is here for the same reason Ruby's and Python's are: a relative
/// URI is resolved against *where the file is*, and the core must not be the
/// layer that turns a path into an import target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DartHeader {
    /// Repo-relative, `/`-separated path of the file.
    pub rel_path: String,
    /// Every URI a directive names, in source order.
    pub uris: Vec<UriSpec>,
}

/// The owner chain of a declaration node, outermost first.
///
/// `None` when the node is inside a body — a local function, a class declared
/// in a closure — or under a container this file gives no name, which is what
/// keeps a node from being invented for a declaration nothing can name.
fn owner_of(node: &SgNode) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for a in node.ancestors() {
        let kind = a.kind();
        if BODIES.contains(&&*kind) {
            return None;
        }
        if matches!(&*kind, "class_body" | "extension_body" | "enum_body") {
            // The body's parent is the declaration that owns it.
            out.push(declared_name(&a.parent()?)?);
        }
    }
    out.reverse();
    Some(out)
}

/// The name a type-level declaration declares, as written.
///
/// `None` for an `extension` with no name — the one Dart declaration that
/// really has none.
fn declared_name(node: &SgNode) -> Option<String> {
    match &*node.kind() {
        "class_declaration"
        | "mixin_declaration"
        | "enum_declaration"
        | "extension_declaration" => node
            .children()
            .find(|c| c.kind() == "identifier")
            .map(|c| c.text().to_string()),
        "extension_type_declaration" => node
            .children()
            .find(|c| c.kind() == "extension_type_name")
            .and_then(|n| {
                n.children()
                    .find(|c| c.kind() == "identifier")
                    .map(|c| c.text().to_string())
            }),
        "type_alias" => node
            .children()
            .find(|c| c.kind() == "type_identifier")
            .map(|c| c.text().to_string()),
        _ => None,
    }
}

/// The modifier keywords written on the declaration this node is part of.
fn modifiers(node: &SgNode) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in node.ancestors() {
        if !WRAPPERS.contains(&&*a.kind()) {
            break;
        }
        for child in a.children() {
            let kind = child.kind().to_string();
            if MODIFIERS.contains(&kind.as_str()) && !out.contains(&kind) {
                out.push(kind);
            }
        }
    }
    out
}

/// Whether a member signature carries no body, and so is abstract.
///
/// The grammar says it structurally: a member with a body is wrapped in a
/// `method_declaration`, and one without is wrapped in a bare `declaration`.
fn is_abstract(node: &SgNode) -> bool {
    node.parent().is_some_and(|p| p.kind() == "declaration")
}

/// The facets a Dart declaration carries.
///
/// [`DefFacets::EXPORTED`] is the visibility, and Dart spells it in the name:
/// a leading `_` is private to the library. See [`crate::track_dart::lang`]
/// for why [`DefFacets::PRIVATE`] is deliberately not set.
fn facets(name: &str, mods: &[String], abstract_: bool) -> DefFacets {
    let mut out = DefFacets::default();
    if !name.starts_with('_') {
        out = out.union(DefFacets::EXPORTED);
    }
    if mods.iter().any(|m| m == "static") {
        out = out.union(DefFacets::STATIC);
    }
    if abstract_ || mods.iter().any(|m| m == "abstract") {
        out = out.union(DefFacets::ABSTRACT);
    }
    out
}

/// One definition, with the fields every Dart declaration shares.
///
/// `params` is `None` throughout: Dart has no overloading, so no reference
/// site here discriminates by arity.
fn def(
    kind: DefKind,
    name: String,
    owner: Vec<String>,
    space: DeclSpace,
    facets: DefFacets,
    span: Span,
) -> Definition {
    Definition {
        kind,
        name,
        owner,
        space,
        facets,
        params: None,
        span,
    }
}

/// A string literal's value, or `None` when it is not one plain literal.
///
/// Adjacent literals are one string — `'a' 'b.dart'` is `ab.dart`, which is
/// how Dart's own grammar reads it. An interpolation answers `None`: a URI
/// this function cannot read is one the resolver must refuse to guess.
fn string_literal(node: &SgNode) -> Option<String> {
    match &*node.kind() {
        "string_literal" => {
            let mut out = String::new();
            for child in node.children() {
                out.push_str(&string_literal(&child)?);
            }
            Some(out)
        }
        kind if kind.starts_with("string_literal_") || kind.starts_with("raw_string_literal_") => {
            let mut out = String::new();
            for child in node.children() {
                match &*child.kind() {
                    k if k.starts_with("template_chars") => out.push_str(&child.text()),
                    // The quote tokens, single and triple, plain and raw.
                    k if k.chars().all(|c| c == '\'' || c == '"' || c == 'r') => {}
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// The URI a `uri` node spells.
fn uri_form(node: &SgNode) -> UriForm {
    node.children()
        .find_map(|c| string_literal(&c))
        .map_or(UriForm::Dynamic, UriForm::Literal)
}

/// Every `uri` node under a directive, in source order.
fn uri_nodes<'r>(node: &SgNode<'r>) -> Vec<SgNode<'r>> {
    let mut out = Vec::new();
    if node.kind() == "uri" {
        out.push(node.clone());
        return out;
    }
    for child in node.children() {
        out.extend(uri_nodes(&child));
    }
    out
}

/// The names a directive's `show`/`hide` combinators list, in source order.
fn combinator_names(node: &SgNode) -> Vec<String> {
    let mut out = Vec::new();
    if node.kind() == "combinator" {
        out.extend(
            node.children()
                .filter(|c| c.kind() == "identifier")
                .map(|c| c.text().to_string()),
        );
        return out;
    }
    for child in node.children() {
        out.extend(combinator_names(&child));
    }
    out
}

/// The `as <prefix>` an import binds, when it binds one.
fn import_prefix(node: &SgNode) -> Option<String> {
    let mut seen_as = false;
    for child in node.children() {
        match &*child.kind() {
            "import_specification" => return import_prefix(&child),
            "as" => seen_as = true,
            "identifier" if seen_as => return Some(child.text().to_string()),
            _ => {}
        }
    }
    None
}

/// Extract one Dart file. The whole of the extractor's public surface.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<DartLang> {
    static RULES: OnceLock<Rules> = OnceLock::new();
    let rules = RULES.get_or_init(|| Rules::compile(DART_RULES).expect("dart.yml compiles"));

    let mut facts: FileFacts<DartLang> = FileFacts {
        header: DartHeader {
            rel_path: rel_path.to_string(),
            uris: Vec::new(),
        },
        defs: Vec::new(),
        refs: Vec::new(),
    };

    // The file's own library node, first, because the driver reads the first
    // `Module` definition as the file's container. Every `.dart` file is a
    // library whether or not it declares anything: an `import` naming an
    // empty file still resolves.
    let stem = rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path)
        .strip_suffix(".dart")
        .unwrap_or("");
    facts.defs.push(def(
        DefKind::Module,
        stem.to_string(),
        Vec::new(),
        DeclSpace::Namespace,
        DefFacets::SYNTHETIC,
        Span {
            byte_start: 0,
            byte_end: source.len() as u32,
            line: 1,
        },
    ));

    let tree = SourceTree::parse_dart(source);
    for (rule, node) in tree.matches(rules) {
        match rule {
            "directive" => directive(&mut facts, &node),
            "def-class" => type_declaration(&mut facts, &node, DefFacets::default()),
            "def-enum" => type_declaration(&mut facts, &node, DefFacets::ENUM),
            "def-typedef" => type_declaration(&mut facts, &node, DefFacets::default()),
            "def-enum-constant" => enum_constant(&mut facts, &node),
            "def-function" => callable(&mut facts, &node, DefKind::Function, DefKind::Method),
            // A getter and a setter are the two halves of one property,
            // at the top level of a library as much as inside a type.
            "def-getter" | "def-setter" => {
                callable(&mut facts, &node, DefKind::Property, DefKind::Property)
            }
            "def-operator" => operator(&mut facts, &node),
            "def-constructor" => constructor(&mut facts, &node),
            "def-variable" => variable(&mut facts, &node),
            _ => {}
        }
    }
    // Rules run one at a time, so the records arrive rule-major; source order
    // is what a reader of a report expects and what a span-keyed pairing needs
    // to be stable under.
    facts.defs[1..].sort_by_key(|d| d.span.byte_start);
    facts.refs.sort_by_key(|r| r.span.byte_start);
    facts.header.uris.sort_by_key(|u| u.span.byte_start);
    facts
}

/// `import`, `export`, `part` and `part of`: one reference per URI named.
fn directive(facts: &mut FileFacts<DartLang>, node: &SgNode) {
    let (kind, keyword) = match &*node.kind() {
        "library_import" => (RefKind::Import, "import"),
        "library_export" => (RefKind::Export, "export"),
        "part_directive" => (RefKind::Import, "part"),
        "part_of_directive" => (RefKind::Import, "part of"),
        _ => return,
    };
    let combinators = combinator_names(node);
    let prefix = import_prefix(node);
    for uri in uri_nodes(node) {
        let form = uri_form(&uri);
        let span = span_of(&uri);
        // The literal text at the site, which is what a `RefKey` is keyed on
        // and what a query prints back. The URI as written plus the binding it
        // introduces, so that two imports of one library under two prefixes
        // stay two rows.
        let mut raw_target = format!("{keyword} {}", uri.text());
        if let Some(prefix) = &prefix {
            raw_target.push_str(&format!(" as {prefix}"));
        }
        let target = match &form {
            UriForm::Literal(path) => RefTarget {
                root: TargetRoot::Name,
                segments: vec![path.clone()],
            },
            // The root is not a name: an interpolated URI is exactly the shape
            // `TargetRoot::Expr` exists for.
            UriForm::Dynamic => RefTarget {
                root: TargetRoot::Expr,
                segments: Vec::new(),
            },
        };
        facts.header.uris.push(UriSpec {
            form,
            directive: keyword,
            combinators: combinators.clone(),
            span,
        });
        facts.refs.push(Reference {
            kind,
            space: DeclSpace::Namespace,
            raw_target,
            target,
            // Tier 2 emits no expression-level reference, so nothing here can
            // name a local: `LocalBinding` does not apply to this track.
            locally_bound: false,
            argc: None,
            // A Dart directive sits at the top of its library and inside no
            // declaration at all, so the driver sources its edge at the
            // library node — which is exactly what imports it.
            enclosing: None,
            span,
        });
    }
}

/// `class`, `mixin`, `enum`, `extension`, `extension type` and `typedef`.
///
/// A `typedef` is a [`DefKind::Type`] and not a [`DefKind::Alias`]: an alias
/// node promises a forward to the definition it names, and the name it names
/// is a *type use*, which this tier does not resolve. Recording it as an alias
/// with nothing to forward to would claim a link that was never made.
fn type_declaration(facts: &mut FileFacts<DartLang>, node: &SgNode, extra: DefFacets) {
    let Some(owner) = owner_of(node) else { return };
    let Some(name) = declared_name(node) else {
        return; // an unnamed `extension`: nothing can name it
    };
    let mods = modifiers(node);
    let mut facets = facets(&name, &mods, false).union(extra);
    if node.children().any(|c| c.kind() == "abstract") {
        facets = facets.union(DefFacets::ABSTRACT);
    }
    facts.defs.push(def(
        DefKind::Type,
        name,
        owner,
        DeclSpace::Type,
        facets,
        span_of(node),
    ));
}

/// `enum E { red, green }`: one constant per name.
fn enum_constant(facts: &mut FileFacts<DartLang>, node: &SgNode) {
    let Some(owner) = owner_of(node) else { return };
    let Some(name) = node
        .children()
        .find(|c| c.kind() == "identifier")
        .map(|c| c.text().to_string())
    else {
        return;
    };
    facts.defs.push(def(
        DefKind::Const,
        name.clone(),
        owner,
        DeclSpace::Value,
        facets(&name, &[], false).union(DefFacets::STATIC),
        span_of(node),
    ));
}

/// A function, a method, a getter or a setter — whichever the owner decides.
fn callable(facts: &mut FileFacts<DartLang>, node: &SgNode, free: DefKind, member: DefKind) {
    let Some(owner) = owner_of(node) else { return };
    let Some(name) = node
        .children()
        .find(|c| c.kind() == "identifier")
        .map(|c| c.text().to_string())
    else {
        return;
    };
    let mods = modifiers(node);
    let kind = if owner.is_empty() { free } else { member };
    facts.defs.push(def(
        kind,
        name.clone(),
        owner,
        DeclSpace::Value,
        facets(&name, &mods, is_abstract(node)),
        span_of(node),
    ));
}

/// `bool operator ==(Object other)`: the method a symbol names.
fn operator(facts: &mut FileFacts<DartLang>, node: &SgNode) {
    let Some(owner) = owner_of(node) else { return };
    if owner.is_empty() {
        return; // Dart declares no top-level operator
    }
    // Everything between the `operator` keyword and the parameter list is the
    // symbol, `[]=` included.
    let mut name = String::new();
    let mut seen = false;
    for child in node.children() {
        match &*child.kind() {
            "operator" => seen = true,
            "formal_parameter_list" => break,
            _ if seen => name.push_str(&child.text()),
            _ => {}
        }
    }
    if name.is_empty() {
        return;
    }
    let mods = modifiers(node);
    facts.defs.push(def(
        DefKind::Method,
        name.clone(),
        owner,
        DeclSpace::Value,
        facets(&name, &mods, is_abstract(node)),
        span_of(node),
    ));
}

/// `C(…)`, `C.named(…)`, `factory C.of(…)`.
///
/// Named with Dart's own tear-off spelling: the unnamed constructor is
/// `C.new`, which is what `C.new` means in source since 2.15, and cannot
/// collide with a method because `new` is a reserved word.
fn constructor(facts: &mut FileFacts<DartLang>, node: &SgNode) {
    let Some(owner) = owner_of(node) else { return };
    if owner.is_empty() {
        return;
    }
    let names: Vec<String> = node
        .children()
        .filter(|c| c.kind() == "identifier")
        .map(|c| c.text().to_string())
        .collect();
    // `[type]` for the unnamed constructor, `[type, name]` for a named one.
    let name = names.get(1).cloned().unwrap_or_else(|| "new".to_string());
    let mods = modifiers(node);
    facts.defs.push(def(
        DefKind::Constructor,
        name.clone(),
        owner,
        DeclSpace::Value,
        facets(&name, &mods, false),
        span_of(node),
    ));
}

/// A field, a top-level variable or a top-level constant — one per declared
/// name, because `int x = 0, y = 1;` declares two.
fn variable(facts: &mut FileFacts<DartLang>, node: &SgNode) {
    let Some(owner) = owner_of(node) else { return };
    let name = node
        .children()
        .find(|c| c.kind() == "identifier")
        .map(|c| c.text().to_string())
        .unwrap_or_else(|| {
            node.text()
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        });
    if name.is_empty() {
        return;
    }
    let mods = modifiers(node);
    let kind = if !owner.is_empty() {
        DefKind::Field
    } else if mods.iter().any(|m| m == "const") {
        DefKind::Const
    } else {
        DefKind::Var
    };
    facts.defs.push(def(
        kind,
        name.clone(),
        owner,
        DeclSpace::Value,
        facets(&name, &mods, false),
        span_of(node),
    ));
}

/// The Dart extractor, as the driver holds it.
pub struct DartExtractor;

impl Extractor<DartLang> for DartExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<DartLang> {
        extract(rel_path, source)
    }
}
