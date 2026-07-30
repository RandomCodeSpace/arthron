//! The EcmaScript extractor: one file in, records out. Forbidden from
//! linking.
//!
//! Two grammars, one interpretation. `rules/javascript.yml` and
//! `rules/typescript.yml` select node kinds and share rule ids wherever the
//! grammars agree, so a single `match` below reads both; the ids only
//! TypeScript has are the delta.
//!
//! What the extractor may say about a module is *what this file writes*.
//! `export * from './x'` is recorded as an export entry naming `./x`, never
//! as the set of names it contributes — `GetExportedNames` recurses into the
//! module graph, and the extractor has one file.
//!
//! # Deliberate omissions
//!
//! Recorded rather than hidden, because an undercount that is known is a work
//! item and an undercount that is not is a lie:
//!
//! - **Dotted property reads are references.** The extractor emits the
//!   outermost `member_expression` once as [`RefKind::FieldAccess`]. A member
//!   in call/new position is already that site's `Call`/`New` reference, and
//!   a plain assignment target writes without reading, so neither is emitted
//!   twice. Computed `a[b]` stays absent: the key has no static name.
//! - **`Object.assign(C.prototype, {…})` and `util.inherits`** are ordinary
//!   calls with runtime meaning; no rule pretends otherwise.
//! - **Transpiler output** (`__exportStar`, `_interopRequireDefault`) is not
//!   pattern-matched. The directories that hold it are skipped instead.
//! - **`export as namespace jQuery`** is not emitted. It is a *declaration*,
//!   not a reference: it names nothing, it makes this module's exports
//!   reachable as a global in script files, and the owner it would need —
//!   the global scope — is not something a module-path-rooted FQN can spell.
//!   Nothing is dropped by omitting it; what is lost is that a global
//!   `jQuery.foo()` in a script file resolves to nothing, and that lands in
//!   an honest reason. Recorded as a gap, not as a claim.
//!   (A `/// <reference … />` directive *is* a reference and is emitted; only
//!   `lib=` is skipped, because it names a compiler library no repository
//!   holds.)
//! - **Computed names** (`class C { [k](){} }`, `obj[name] = fn`) are not
//!   definitions. They have no static name, and naming them from the
//!   expression text would invent one.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_ecma::bind::{
    is_function_like, is_locally_bound, module_scope_binds, pattern_names,
};
use crate::track_ecma::lang::{
    Dialect, EcmaHeader, ExportEntry, ImportBinding, ImportSyntax, ImportedName, JsLang,
    ModuleImport, ModuleKind, ModuleKindSource, TsLang, space_tag,
};
use crate::track_ecma::resolve::PROTOTYPE;

/// The embedded JavaScript extraction rules.
const JS_RULES: &str = include_str!("../rules/javascript.yml");
/// The embedded TypeScript extraction rules.
const TS_RULES: &str = include_str!("../rules/typescript.yml");

/// The synthetic local name of an anonymous default export, and of the
/// CommonJS `module.exports` object.
///
/// ES binds `export default <expression>` to exactly this name, and it is
/// ideal precisely because it is not a valid `IdentifierName`: no real
/// declaration can collide with it. CommonJS reuses it because
/// `module.exports` *is* the default export under ESM interop, so one
/// synthetic covers both eras.
pub const DEFAULT_LOCAL: &str = "*default*";

/// The export name TypeScript's `export = X` occupies.
pub const EXPORT_EQUALS: &str = "export=";

/// The owner a `declare global { … }` body's declarations belong to.
///
/// A global augmentation places a declaration outside its file's module, so
/// its owner cannot be derived from the path. Not a valid identifier, for the
/// same reason [`DEFAULT_LOCAL`] is not.
pub const GLOBAL_OWNER: &str = "<global>";

/// The reserved name of the marker recording that a module's export set is
/// not enumerable from this file.
///
/// `export * from './x'` makes the set a fixed point over the module graph
/// (ES `GetExportedNames`) and a `module.exports` spread makes it a runtime
/// value. Either way a later lookup that misses must be able to say "the set
/// could not be enumerated" rather than "the name is absent", and this is the
/// node that lets it. `*` is not a valid `IdentifierName`, so it collides
/// with nothing written.
pub const STAR_EXPORT: &str = "*";

/// The JavaScript extractor. Stateless.
pub struct JsExtractor;

/// The TypeScript extractor. Stateless.
pub struct TsExtractor;

impl Extractor<JsLang> for JsExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<JsLang> {
        let facts = extract(Dialect::JavaScript, rel_path, source);
        FileFacts {
            header: facts.header,
            defs: facts.defs,
            refs: facts.refs,
        }
    }
}

impl Extractor<TsLang> for TsExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<TsLang> {
        let facts = extract(Dialect::TypeScript, rel_path, source);
        FileFacts {
            header: facts.header,
            defs: facts.defs,
            refs: facts.refs,
        }
    }
}

/// Everything one file says, before it is wrapped for a `Language`.
///
/// The two `Language` impls share every field, so the work is done once and
/// the wrapper is the only thing that differs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EcmaFacts {
    /// Per-file facts only the resolver reads.
    pub header: EcmaHeader,
    /// Declarations the file makes.
    pub defs: Vec<Definition>,
    /// Sites that name something possibly defined elsewhere.
    pub refs: Vec<Reference>,
}

fn js_rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules::compile(JS_RULES).expect("embedded javascript.yml compiles"))
}

fn ts_rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules::compile(TS_RULES).expect("embedded typescript.yml compiles"))
}

/// A container a definition is declared in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Owner {
    /// Owner chain, outermost first. Empty at module level.
    path: Vec<String>,
    /// The space members of this container land in, when the container
    /// fixes one. `None` means the declaration's own natural space — a
    /// namespace holds values *and* types.
    space: Option<DeclSpace>,
}

/// The unquoted content of a string literal, when the node is one.
///
/// A template with no substitution is a literal too: `` require(`./x`) ``
/// names exactly `./x`.
fn literal_text(node: &SgNode) -> Option<String> {
    match &*node.kind() {
        "string" => Some(
            node.children()
                .filter(|c| c.kind() == "string_fragment")
                .map(|c| c.text().to_string())
                .collect(),
        ),
        "template_string" => {
            if node.children().any(|c| c.kind() == "template_substitution") {
                return None;
            }
            Some(
                node.children()
                    .filter(|c| c.kind() == "string_fragment")
                    .map(|c| c.text().to_string())
                    .collect(),
            )
        }
        _ => None,
    }
}

/// A node's `name` field as a `String`.
fn name_of(node: &SgNode) -> Option<String> {
    node.field("name").map(|n| n.text().to_string())
}

/// Whether a declaration sits directly under an `export` statement.
fn is_exported(node: &SgNode) -> bool {
    node.parent()
        .is_some_and(|p| p.kind() == "export_statement")
}

/// Whether a node has a direct child of this kind — how the grammar spells
/// the modifiers `static`, `abstract`, `const`, `declare`.
fn has_child(node: &SgNode, kind: &str) -> bool {
    node.children().any(|c| c.kind() == kind)
}

/// The declared name of a class-like node, falling back to the variable it
/// initialises.
///
/// `const C = class { m(){} }` names its members exactly as `class C` does,
/// so the members are nodes either way.
fn class_name(node: &SgNode) -> Option<String> {
    if let Some(name) = name_of(node) {
        return Some(name);
    }
    let parent = node.parent()?;
    if parent.kind() == "variable_declarator" {
        return parent.field("name").and_then(|n| match &*n.kind() {
            "identifier" => Some(n.text().to_string()),
            _ => None,
        });
    }
    None
}

/// The segments of `namespace A.B.C`.
///
/// Only `A` is bound in the enclosing scope; the nesting is sugar, and the
/// extractor unfolds it so the whole path is nameable.
fn namespace_segments(node: &SgNode) -> Vec<String> {
    match node.field("name") {
        Some(n) => n.text().split('.').map(str::to_string).collect(),
        None => Vec::new(),
    }
}

/// The specifier a `declare module "foo"` names.
fn ambient_module_name(node: &SgNode) -> Option<String> {
    node.children().find_map(|c| literal_text(&c))
}

/// The container chain around a node, or `None` when the node sits in a
/// function or a block — where declarations are locals and locals are not
/// nodes.
fn owner_of(node: &SgNode) -> Option<Owner> {
    owner_chain(node, false)
}

/// The container chain a `var` declaration's names belong to.
///
/// D3: ES puts `var` and function declarations in `VarScopedDeclarations`,
/// which are instantiated in the nearest **function or module** environment —
/// never in the block they are written in. So `{ var f = () => {} }` at module
/// level declares a module-level binding, and a module-level binding is a
/// node; the identical `let` is a local. Blocks, loop heads, `switch` bodies
/// and `catch` clauses are therefore transparent here and opaque in
/// [`owner_of`]. A class static block is not: it is a function environment.
fn var_owner_of(node: &SgNode) -> Option<Owner> {
    owner_chain(node, true)
}

fn owner_chain(node: &SgNode, var_scoped: bool) -> Option<Owner> {
    let ancestors: Vec<SgNode> = node.ancestors().collect();
    let mut path: Vec<String> = Vec::new();
    let mut space: Option<DeclSpace> = None;
    for (i, ancestor) in ancestors.iter().enumerate() {
        let parent = ancestors.get(i + 1);
        match &*ancestor.kind() {
            "program" => break,
            "class_body" => {
                path.push(class_name(parent?)?);
                space.get_or_insert(DeclSpace::Value);
            }
            "interface_body" => {
                path.push(name_of(parent?)?);
                space.get_or_insert(DeclSpace::Type);
            }
            "enum_body" => {
                path.push(name_of(parent?)?);
                space.get_or_insert(DeclSpace::Value);
            }
            "internal_module" => {
                for segment in namespace_segments(ancestor).into_iter().rev() {
                    path.push(segment);
                }
            }
            "statement_block" => match parent.map(|p| p.kind()) {
                // A namespace body: its declarations are nodes, and the
                // `internal_module` ancestor supplies the name.
                Some(kind) if &*kind == "internal_module" => {}
                Some(kind) if &*kind == "module" => path.push(ambient_module_name(parent?)?),
                Some(kind) if &*kind == "ambient_declaration" => {
                    if has_child(parent?, "global") {
                        path.push(GLOBAL_OWNER.to_string());
                    }
                }
                // Any other block is a scope, not a container — except to a
                // `var`, which hoists straight through it.
                _ if var_scoped => {}
                _ => return None,
            },
            // An object literal is a container only where the node rule says
            // so, and the caller says it — never by ancestry, or every
            // options object in the corpus becomes one.
            "object" => return None,
            // A static block is a function environment, so a `var` in one
            // stays local to it.
            "class_static_block" => return None,
            "for_statement" | "for_in_statement" | "catch_clause" | "switch_body"
            | "switch_case" | "switch_default"
                if !var_scoped =>
            {
                return None;
            }
            kind if is_function_like(kind) => return None,
            _ => {}
        }
    }
    path.reverse();
    Some(Owner { path, space })
}

/// Build a definition's facets.
///
/// `RUNTIME` marks what survives to the emitted JavaScript. Its negation is
/// what keeps erased constructs — interfaces, type aliases, `const enum`,
/// uninstantiated namespaces — out of the call graph while leaving them
/// nodes in the type space.
fn def_facets(space: DeclSpace, exported: bool, runtime: bool, extra: DefFacets) -> DefFacets {
    let mut facets = extra;
    if runtime {
        facets = facets.union(DefFacets::RUNTIME);
    }
    if exported {
        facets = facets.union(DefFacets::EXPORTED);
    }
    let _ = space;
    facets
}

/// Assemble one definition.
#[allow(clippy::too_many_arguments)]
fn def(
    kind: DefKind,
    name: impl Into<String>,
    owner: Vec<String>,
    space: DeclSpace,
    facets: DefFacets,
    span: Span,
) -> Definition {
    Definition {
        kind,
        name: name.into(),
        owner,
        space,
        facets,
        // ECMAScript has no signature-based dispatch anywhere: a later
        // declaration replaces an earlier one, and a TypeScript overload set
        // has one implementation and one identity. An arity component in the
        // FQN would create nodes no reference can name.
        params: None,
        span,
    }
}

/// The number of arguments at a call or construction site.
///
/// A spread counts as one: the language does not discriminate by arity, so
/// this is a dedup key component rather than a resolution input. `Some(0)`
/// and `None` are different facts and stay different keys.
fn argument_count(node: &SgNode) -> Option<u32> {
    let list = node.field("arguments")?;
    let count = list
        .children()
        .filter(|c| c.is_named() && c.kind() != "comment")
        .count();
    u32::try_from(count).ok()
}

/// Parse a member-access chain into a target shape.
///
/// The chain is walked to its innermost operand: an identifier there makes
/// the whole dotted path a [`TargetRoot::Name`] target, `this`/`super` make
/// it their own root, and anything else — a call result, a computed index —
/// makes it [`TargetRoot::Expr`] carrying only the trailing selectors. The
/// *number* of segments is what the resolver dispatches on, so a three-deep
/// chain stays distinguishable from a qualified name.
fn member_target(node: &SgNode) -> RefTarget {
    let mut segments: Vec<String> = Vec::new();
    let mut current = node.clone();
    loop {
        match &*current.kind() {
            "identifier"
            | "shorthand_property_identifier"
            | "property_identifier"
            | "private_property_identifier"
            | "type_identifier" => {
                segments.push(current.text().to_string());
                segments.reverse();
                return RefTarget {
                    root: TargetRoot::Name,
                    segments,
                };
            }
            "this" => {
                segments.reverse();
                return RefTarget {
                    root: TargetRoot::This { qualifier: vec![] },
                    segments,
                };
            }
            "super" => {
                segments.reverse();
                return RefTarget {
                    root: TargetRoot::Super { qualifier: vec![] },
                    segments,
                };
            }
            "member_expression" => {
                let (Some(object), Some(property)) =
                    (current.field("object"), current.field("property"))
                else {
                    break;
                };
                segments.push(property.text().to_string());
                current = object;
            }
            "nested_identifier" | "nested_type_identifier" => {
                let mut parts: Vec<String> =
                    current.text().split('.').map(str::to_string).collect();
                parts.extend(segments.into_iter().rev());
                return RefTarget {
                    root: TargetRoot::Name,
                    segments: parts,
                };
            }
            "parenthesized_expression" | "non_null_expression" => {
                let inner = current.children().find(|c| c.is_named());
                match inner {
                    Some(inner) => current = inner,
                    None => break,
                }
            }
            _ => break,
        }
    }
    segments.reverse();
    RefTarget {
        root: TargetRoot::Expr,
        segments,
    }
}

/// The nearest *nameable* enclosing definition of a reference site.
///
/// Anonymous functions and arrows are skipped: they are not nodes, so a call
/// inside one belongs to the named definition around it. A named declaration
/// that is itself inside a function is skipped for the same reason — the
/// walk continues outward until it finds one the graph can hold.
///
/// The returned path carries a reserved space tag whenever the enclosing
/// declaration is not in the Value space. [`Encloser`] has no space field and
/// [`Encloser::as_definition`] hardcodes [`DeclSpace::Value`], so without the
/// tag an edge out of an `interface` body would be sourced at an identity no
/// node has — see the FQN grammar's fourth invariant in
/// [`crate::track_ecma::resolve`].
fn enclosing_definition(node: &SgNode) -> Option<Encloser> {
    for ancestor in node.ancestors() {
        let found = match &*ancestor.kind() {
            "method_definition" | "abstract_method_signature" | "method_signature" => {
                member_name(&ancestor).map(|name| (name, method_kind(&ancestor)))
            }
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                name_of(&ancestor).map(|name| (name, DefKind::Function))
            }
            "field_definition" | "public_field_definition" => {
                member_name(&ancestor).map(|name| (name, DefKind::Method))
            }
            "variable_declarator" => match ancestor.field("name") {
                Some(n) if n.kind() == "identifier" => {
                    Some((n.text().to_string(), DefKind::Function))
                }
                _ => None,
            },
            "class_declaration" | "abstract_class_declaration" => {
                class_name(&ancestor).map(|name| (name, DefKind::Type))
            }
            "interface_declaration" | "type_alias_declaration" | "enum_declaration" => {
                name_of(&ancestor).map(|name| (name, DefKind::Type))
            }
            "internal_module" => namespace_segments(&ancestor)
                .last()
                .cloned()
                .map(|name| (name, DefKind::Module)),
            _ => None,
        };
        let Some((name, kind)) = found else {
            continue;
        };
        // A `var` declarator is var-scoped, so an edge out of a module-level
        // `var f = () => {}` written inside a block starts at the node that
        // declaration made rather than at nothing.
        let owner = if ancestor.kind() == "variable_declarator"
            && ancestor
                .parent()
                .is_some_and(|p| p.kind() == "variable_declaration")
        {
            var_owner_of(&ancestor)
        } else {
            owner_of(&ancestor)
        };
        let Some(owner) = owner else {
            continue;
        };
        // The declaration's own space when the ancestor kind fixes one, else
        // its container's — an interface member is a Type-space declaration
        // because `interface_body` is a Type-space container.
        let space = match &*ancestor.kind() {
            "interface_declaration" | "type_alias_declaration" => DeclSpace::Type,
            "internal_module" => DeclSpace::Namespace,
            _ => owner.space.unwrap_or(DeclSpace::Value),
        };
        let mut path: Vec<String> = space_tag(space).into_iter().map(str::to_string).collect();
        path.extend(owner.path);
        // The same prototype segment `class_members` gives the definition, so
        // the edge out of a method and the node it starts at agree — and so
        // `this.m()` inside an instance method looks the member up on the
        // prototype while `this.m()` inside a static one looks it up on the
        // constructor, which is what the two mean.
        if is_instance_class_member(&ancestor) {
            path.push(PROTOTYPE.to_string());
        }
        path.push(name);
        return Some(Encloser { path, kind });
    }
    None
}

/// Whether a node is a class element installed on the prototype rather than
/// on the constructor.
///
/// Written directly under a `class_body`, and without `static`. An interface
/// or object-type member is not one: it has no constructor to be static on,
/// so its container needs no discriminator.
fn is_instance_class_member(node: &SgNode) -> bool {
    matches!(
        &*node.kind(),
        "method_definition"
            | "abstract_method_signature"
            | "method_signature"
            | "field_definition"
            | "public_field_definition"
    ) && node.parent().is_some_and(|p| p.kind() == "class_body")
        && !has_child(node, "static")
}

/// A class or interface member's declared name, or `None` when it is
/// computed and therefore not nameable.
fn member_name(node: &SgNode) -> Option<String> {
    let name = node
        .field("name")
        .or_else(|| node.field("key"))
        .or_else(|| node.field("property"))?;
    match &*name.kind() {
        "property_identifier" | "private_property_identifier" => Some(name.text().to_string()),
        "string" => literal_text(&name),
        _ => None,
    }
}

/// Which kind of member a `method_definition` declares.
fn method_kind(node: &SgNode) -> DefKind {
    if has_child(node, "get") || has_child(node, "set") {
        return DefKind::Property;
    }
    if node
        .field("name")
        .is_some_and(|n| n.text() == "constructor")
    {
        return DefKind::Constructor;
    }
    DefKind::Method
}

/// Whether an initialiser makes the binding callable.
fn is_callable_value(node: &SgNode) -> bool {
    matches!(
        &*node.kind(),
        "arrow_function" | "function_expression" | "function" | "generator_function"
    )
}

/// Whether an initialiser is a class.
fn is_class_value(node: &SgNode) -> bool {
    matches!(&*node.kind(), "class" | "class_expression")
}

/// Accumulator for one file's facts.
struct Ctx {
    dialect: Dialect,
    header: EcmaHeader,
    defs: Vec<Definition>,
    refs: Vec<Reference>,
    /// Whether an `import`/`export` declaration was seen — a parse-level ESM
    /// marker, and the only one a file carries.
    esm_syntax: bool,
    /// Whether a CommonJS export idiom or a `require` call was seen.
    cjs_syntax: bool,
}

impl Ctx {
    fn push_def(
        &mut self,
        kind: DefKind,
        name: impl Into<String>,
        owner: Vec<String>,
        space: DeclSpace,
        facets: DefFacets,
        span: Span,
    ) {
        self.defs.push(def(kind, name, owner, space, facets, span));
    }

    fn push_ref(
        &mut self,
        kind: RefKind,
        space: DeclSpace,
        node: &SgNode,
        raw_target: String,
        target: RefTarget,
        argc: Option<u32>,
    ) {
        let locally_bound = match (&target.root, target.segments.first()) {
            (TargetRoot::Name, Some(root)) => is_locally_bound(node, root, space),
            _ => false,
        };
        self.refs.push(Reference {
            kind,
            space,
            raw_target,
            target,
            locally_bound,
            argc,
            arg_types: None,
            enclosing: enclosing_definition(node),
            span: span_of(node),
        });
    }

    fn push_export(&mut self, entry: ExportEntry) {
        self.header.exports.push(entry);
    }

    /// Record a module-naming site: the header fact and the reference that
    /// goes with it. Both halves always exist — the reference is the
    /// extractor's, and the binding effect it has is the resolver's.
    fn push_import(
        &mut self,
        node: &SgNode,
        syntax: ImportSyntax,
        specifier: Option<String>,
        raw_specifier: String,
        bindings: Vec<ImportBinding>,
        space: DeclSpace,
    ) {
        let span = span_of(node);
        let target = match &specifier {
            Some(s) => RefTarget {
                root: TargetRoot::Name,
                segments: vec![s.clone()],
            },
            None => RefTarget {
                root: TargetRoot::Expr,
                segments: vec![],
            },
        };
        self.refs.push(Reference {
            kind: RefKind::Import,
            space,
            raw_target: raw_specifier.clone(),
            target,
            locally_bound: false,
            argc: None,
            arg_types: None,
            enclosing: enclosing_definition(node),
            span,
        });
        self.header.imports.push(ModuleImport {
            specifier,
            raw_specifier,
            syntax,
            bindings,
            span,
        });
    }
}

/// Extract every fact from one EcmaScript file.
pub fn extract(dialect: Dialect, rel_path: &str, source: &str) -> EcmaFacts {
    let (tree, rules) = match dialect {
        Dialect::JavaScript => (SourceTree::parse_javascript(source), js_rules()),
        Dialect::TypeScript => (SourceTree::parse_typescript(source), ts_rules()),
    };
    let mut ctx = Ctx {
        dialect,
        header: EcmaHeader {
            rel_path: rel_path.to_string(),
            ..EcmaHeader::default()
        },
        defs: Vec::new(),
        refs: Vec::new(),
        esm_syntax: false,
        cjs_syntax: false,
    };

    for (rule_id, node) in tree.matches(rules) {
        match rule_id {
            "import-stmt" => import_statement(&mut ctx, &node),
            "export-stmt" => export_statement(&mut ctx, &node),
            "def-function" | "def-signature" => function_declaration(&mut ctx, &node),
            "def-class" => class_declaration(&mut ctx, &node),
            "def-binding" => binding_declaration(&mut ctx, &node),
            "def-assign" => assignment(&mut ctx, &node),
            "def-interface" => interface_declaration(&mut ctx, &node),
            "def-type-alias" => type_alias(&mut ctx, &node),
            "def-enum" => enum_declaration(&mut ctx, &node),
            "def-namespace" => namespace_declaration(&mut ctx, &node),
            "def-ambient" => ambient_declaration(&mut ctx, &node),
            "ref-call" => call(&mut ctx, &node),
            "ref-member" => member_read(&mut ctx, &node),
            "ref-new" => new_expression(&mut ctx, &node),
            "ref-jsx" => jsx_element(&mut ctx, &node),
            "ref-heritage" => heritage(&mut ctx, &node),
            "ref-type" => type_use(&mut ctx, &node),
            "ref-type-query" => type_query(&mut ctx, &node),
            "ref-decorator" => decorator(&mut ctx, &node),
            "ref-triple-slash" => triple_slash(&mut ctx, &node),
            _ => {}
        }
    }

    finish(ctx, rel_path)
}

/// Decide the file's module semantics and prepend its module node.
fn finish(mut ctx: Ctx, rel_path: &str) -> EcmaFacts {
    let extension = rel_path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let (kind, source) = match extension {
        // NODE `ESM_FILE_FORMAT`: the extension is normative for these two.
        "mjs" | "mts" => (ModuleKind::Esm, ModuleKindSource::Extension),
        "cjs" | "cts" => (ModuleKind::CommonJs, ModuleKindSource::Extension),
        _ if ctx.esm_syntax => (ModuleKind::Esm, ModuleKindSource::Syntax),
        _ if ctx.cjs_syntax => (ModuleKind::CommonJs, ModuleKindSource::Syntax),
        // Nothing in the file decided it. The nearest `package.json`
        // `"type"` does, and that is the resolver's input, not a file fact.
        _ => (ModuleKind::Undecided, ModuleKindSource::Undecided),
    };
    ctx.header.module_kind = kind;
    ctx.header.module_kind_source = source;
    // A file with no top-level `import`/`export` is a Script: its top-level
    // declarations reach the global scope, and the sloppy-mode hazards apply
    // only here.
    ctx.header.script = !ctx.esm_syntax;

    // The file *is* the module. Its node exists whether or not anything was
    // parsed, because `import './side-effect.js'` and every extension probe
    // need something to hit. The name is the path: a module FQN never
    // carries the definition separator, so the two namespaces cannot collide.
    ctx.defs.insert(
        0,
        def(
            DefKind::Module,
            rel_path,
            vec![],
            DeclSpace::Namespace,
            DefFacets::RUNTIME.union(DefFacets::EXPORTED),
            Span {
                byte_start: 0,
                byte_end: 0,
                line: 0,
            },
        ),
    );

    // The module's export *surface*, as nodes an importer can probe.
    //
    // Not a link and not a fixed point: each entry is a fact this file wrote
    // about a name it exports. What the name ultimately reaches is the
    // resolver's, and for a re-export it is a hop this core cannot yet
    // record — see `resolve::EcmaResolver::index_keys`.
    let aliases = export_aliases(&ctx.header, &ctx.defs);
    ctx.defs.extend(aliases);

    EcmaFacts {
        header: ctx.header,
        defs: ctx.defs,
        refs: ctx.refs,
    }
}

/// The alias definitions a file's export entries contribute.
///
/// An entry earns a node only when the exported name is not already the name
/// of a module-level declaration in this file. `export function parse(){}`
/// exports the definition under its own name, so the definition *is* the
/// export surface and a second node under the same identity would be one
/// record overwriting the other. `export { p as parse }`, a re-export, and
/// every `default` do need one: no declaration here carries that name.
fn export_aliases(header: &EcmaHeader, defs: &[Definition]) -> Vec<Definition> {
    let declared: Vec<(&str, DeclSpace)> = defs
        .iter()
        .filter(|d| d.owner.is_empty() && d.kind != DefKind::Module)
        .map(|d| (d.name.as_str(), d.space))
        .collect();
    let mut out: Vec<Definition> = Vec::new();
    let mut seen: Vec<(String, DeclSpace)> = Vec::new();
    let push = |name: &str,
                owner: Vec<String>,
                space: DeclSpace,
                span: Span,
                out: &mut Vec<Definition>,
                seen: &mut Vec<(String, DeclSpace)>| {
        let key = (format!("{}{name}", owner.join(".")), space);
        if seen.contains(&key) {
            return;
        }
        seen.push(key);
        out.push(def(
            DefKind::Alias,
            name,
            owner,
            space,
            DefFacets::EXPORTED,
            span,
        ));
    };
    for entry in &header.exports {
        let Some(name) = &entry.export_name else {
            // The bare star, and the CommonJS spread that is its equivalent.
            push(
                STAR_EXPORT,
                vec![],
                entry.space,
                entry.span,
                &mut out,
                &mut seen,
            );
            continue;
        };
        // No `AmbiguousExport` marker is minted here, deliberately. ES
        // `ResolveExport` returns that sentinel only when two *star* exports
        // supply one name from different modules — which needs
        // `GetExportedNames`, a fixed point over the module graph that this
        // build does not run (see `resolve`'s module header). Two explicit
        // exports of one name are a duplicate-export SyntaxError in a module
        // and last-writer-wins in CommonJS, so neither is ambiguous, and a
        // marker for them would be a node no reference can name.
        let is_own_declaration = entry.local_name.as_deref() == Some(name.as_str())
            && declared.contains(&(name.as_str(), entry.space));
        if is_own_declaration {
            continue;
        }
        push(name, vec![], entry.space, entry.span, &mut out, &mut seen);
    }
    out
}

/// Node kinds that make their subtree a type position.
///
/// `import('./m')` inside one of these is TypeScript's import-type node and
/// not a runtime module load, and the distinction decides whether the edge
/// survives erasure.
fn in_type_position(node: &SgNode) -> bool {
    node.ancestors().any(|a| {
        matches!(
            &*a.kind(),
            "type_annotation"
                | "type_alias_declaration"
                | "type_arguments"
                | "type_parameters"
                | "union_type"
                | "intersection_type"
                | "generic_type"
                | "array_type"
                | "object_type"
                | "function_type"
                | "conditional_type"
                | "parenthesized_type"
                | "type_predicate"
                | "index_type_query"
                | "lookup_type"
                | "extends_type_clause"
                | "implements_clause"
                | "opting_type_annotation"
                | "omitting_type_annotation"
        )
    })
}

/// Every name a declaration introduces, with the space each lands in.
fn declared_names(dialect: Dialect, node: &SgNode) -> Vec<(String, DeclSpace)> {
    let ts = dialect == Dialect::TypeScript;
    match &*node.kind() {
        "function_declaration" | "generator_function_declaration" | "function_signature" => {
            name_of(node).map_or_else(Vec::new, |n| vec![(n, DeclSpace::Value)])
        }
        "class_declaration" | "abstract_class_declaration" | "enum_declaration" => class_name(node)
            .map_or_else(Vec::new, |n| {
                if ts {
                    vec![(n.clone(), DeclSpace::Value), (n, DeclSpace::Type)]
                } else {
                    vec![(n, DeclSpace::Value)]
                }
            }),
        "interface_declaration" | "type_alias_declaration" => {
            name_of(node).map_or_else(Vec::new, |n| vec![(n, DeclSpace::Type)])
        }
        "internal_module" => namespace_segments(node)
            .first()
            .cloned()
            .map_or_else(Vec::new, |n| vec![(n, DeclSpace::Namespace)]),
        "lexical_declaration" | "variable_declaration" => node
            .children()
            .filter(|c| c.kind() == "variable_declarator")
            .filter_map(|d| d.field("name"))
            .flat_map(|p| pattern_names(&p))
            .map(|n| (n, DeclSpace::Value))
            .collect(),
        "ambient_declaration" => node
            .children()
            .filter(|c| c.is_named())
            .flat_map(|c| declared_names(dialect, &c))
            .collect(),
        _ => Vec::new(),
    }
}

/// A local export entry: `export { x as pub }`, `export function f(){}`.
fn local_entry(name: String, local: String, space: DeclSpace, span: Span) -> ExportEntry {
    ExportEntry {
        export_name: Some(name),
        local_name: Some(local),
        module_request: None,
        import_name: None,
        space,
        span,
    }
}

/// A re-export entry: the name comes from another module, and this file
/// creates no local binding for it.
fn indirect_entry(
    name: Option<String>,
    request: Option<String>,
    imported: ImportedName,
    space: DeclSpace,
    span: Span,
) -> ExportEntry {
    ExportEntry {
        export_name: name,
        local_name: None,
        module_request: request,
        import_name: Some(imported),
        space,
        span,
    }
}

/// The reference a re-export makes to the module it names.
///
/// Without it a barrel file produces zero references and the whole re-export
/// layer is invisible to the graph.
fn export_reference(ctx: &mut Ctx, node: &SgNode, specifier: Option<String>, space: DeclSpace) {
    let raw = node
        .children()
        .find(|c| c.kind() == "string")
        .map(|s| s.text().to_string())
        .unwrap_or_default();
    let (raw_target, target) = match specifier {
        Some(s) => (
            s.clone(),
            RefTarget {
                root: TargetRoot::Name,
                segments: vec![s],
            },
        ),
        None => (
            raw,
            RefTarget {
                root: TargetRoot::Expr,
                segments: vec![],
            },
        ),
    };
    ctx.refs.push(Reference {
        kind: RefKind::Export,
        space,
        raw_target,
        target,
        locally_bound: false,
        argc: None,
        arg_types: None,
        enclosing: None,
        span: span_of(node),
    });
}

/// One reference per *name* a re-export forwards, sourced at that name's
/// alias node rather than at the module.
///
/// B7/B10: `export { parse } from './parse.js'` is a reference that is
/// neither a call nor an import, and one reference for the whole statement
/// would leave a barrel's names unlinked — the edge would say only that
/// `index.js` mentions `parse.js`, not that `index.js#value:parse` reaches
/// `parse.js#value:parse`. The encloser is what makes the alias the edge's
/// source: the driver names an edge's source with the same function that
/// names definitions, so the alias node and the edge out of it cannot
/// disagree.
fn reexport_reference(
    ctx: &mut Ctx,
    spec_node: &SgNode,
    specifier: &str,
    imported: &str,
    exported: &str,
    space: DeclSpace,
) {
    let mut path: Vec<String> = space_tag(space).into_iter().map(str::to_string).collect();
    path.push(exported.to_string());
    ctx.refs.push(Reference {
        kind: RefKind::Export,
        space,
        raw_target: specifier.to_string(),
        target: RefTarget {
            root: TargetRoot::Name,
            segments: vec![specifier.to_string(), imported.to_string()],
        },
        locally_bound: false,
        argc: None,
        arg_types: None,
        enclosing: Some(Encloser {
            path,
            kind: DefKind::Alias,
        }),
        span: span_of(spec_node),
    });
}

fn import_statement(ctx: &mut Ctx, node: &SgNode) {
    ctx.esm_syntax = true;
    // A22: `import x = require("m")` — a CommonJS whole-module binding
    // written as an import declaration.
    if let Some(clause) = node
        .children()
        .find(|c| c.kind() == "import_require_clause")
    {
        let local = clause
            .children()
            .find(|c| c.kind() == "identifier")
            .map(|c| c.text().to_string());
        let specifier = clause.children().find_map(|c| literal_text(&c));
        let raw = specifier.clone().unwrap_or_default();
        let bindings = local
            .map(|local| {
                vec![ImportBinding {
                    local,
                    imported: ImportedName::Whole,
                    space: DeclSpace::Value,
                }]
            })
            .unwrap_or_default();
        ctx.push_import(
            node,
            ImportSyntax::ImportEquals,
            specifier,
            raw,
            bindings,
            DeclSpace::Value,
        );
        return;
    }

    let Some(spec_node) = node.children().find(|c| c.kind() == "string") else {
        return;
    };
    let specifier = literal_text(&spec_node);
    let raw = specifier
        .clone()
        .unwrap_or_else(|| spec_node.text().to_string());
    // C17: `import type { … }` is elided from the emitted JavaScript, so the
    // edge it makes is not a runtime dependency.
    let space = if has_child(node, "type") {
        DeclSpace::Type
    } else {
        DeclSpace::Value
    };

    let mut bindings = Vec::new();
    if let Some(clause) = node.children().find(|c| c.kind() == "import_clause") {
        for child in clause.children() {
            match &*child.kind() {
                // F2: the local name of a default import is unrelated to the
                // definition's name. The binding is the only way back.
                "identifier" => bindings.push(ImportBinding {
                    local: child.text().to_string(),
                    imported: ImportedName::Default,
                    space,
                }),
                "namespace_import" => {
                    if let Some(id) = child.children().find(|c| c.kind() == "identifier") {
                        bindings.push(ImportBinding {
                            local: id.text().to_string(),
                            imported: ImportedName::Namespace,
                            space,
                        });
                    }
                }
                "named_imports" => {
                    for spec in child.children().filter(|c| c.kind() == "import_specifier") {
                        let space = if has_child(&spec, "type") {
                            DeclSpace::Type
                        } else {
                            space
                        };
                        let names = specifier_names(&spec);
                        let Some(imported) = names.first().cloned() else {
                            continue;
                        };
                        let local = names.get(1).cloned().unwrap_or_else(|| imported.clone());
                        bindings.push(ImportBinding {
                            local,
                            imported: ImportedName::Named(imported),
                            space,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    ctx.push_import(node, ImportSyntax::Esm, specifier, raw, bindings, space);
}

/// The one or two names an import/export specifier writes, in source order.
///
/// B12: an exported name may be any string, so the second position is not
/// always an identifier.
fn specifier_names(spec: &SgNode) -> Vec<String> {
    spec.children()
        .filter_map(|c| match &*c.kind() {
            "identifier" => Some(c.text().to_string()),
            "string" => literal_text(&c),
            _ => None,
        })
        .collect()
}

fn export_statement(ctx: &mut Ctx, node: &SgNode) {
    // An `export` inside a namespace or an ambient-module body belongs to
    // that container, not to the file: it does not make the file a Module and
    // it contributes nothing to the file's export map. The declaration itself
    // is still a node — its own rule emits it — so leaving here loses no fact,
    // and staying would mint a module-level export alias for a name the module
    // does not export.
    if owner_of(node).is_none_or(|o| !o.path.is_empty()) {
        return;
    }
    ctx.esm_syntax = true;
    let span = span_of(node);
    let space = if has_child(node, "type") {
        DeclSpace::Type
    } else {
        DeclSpace::Value
    };
    let specifier = node
        .children()
        .find(|c| c.kind() == "string")
        .and_then(|s| literal_text(&s));

    // B12: `export = Foo` — the module's export *is* `Foo`.
    if has_child(node, "=") {
        let local = node
            .children()
            .find(|c| c.kind() == "identifier")
            .map(|c| c.text().to_string());
        ctx.push_export(ExportEntry {
            export_name: Some(EXPORT_EQUALS.to_string()),
            local_name: local,
            module_request: None,
            import_name: Some(ImportedName::Whole),
            space: DeclSpace::Value,
            span,
        });
        return;
    }

    // B6: `export * as ns from './u'`. Unlike a bare star, the namespace
    // object includes `default`.
    if let Some(ns) = node.children().find(|c| c.kind() == "namespace_export") {
        let name = ns
            .children()
            .find(|c| c.kind() == "identifier")
            .map(|c| c.text().to_string());
        ctx.push_export(indirect_entry(
            name.clone(),
            specifier.clone(),
            ImportedName::Namespace,
            space,
            span,
        ));
        // B6: the namespace object *is* the exported name, so the edge starts
        // at that alias and lands on the module it wraps.
        match (&name, &specifier) {
            (Some(exported), Some(spec)) => {
                let mut path: Vec<String> =
                    space_tag(space).into_iter().map(str::to_string).collect();
                path.push(exported.clone());
                ctx.refs.push(Reference {
                    kind: RefKind::Export,
                    space,
                    raw_target: spec.clone(),
                    target: RefTarget {
                        root: TargetRoot::Name,
                        segments: vec![spec.clone()],
                    },
                    locally_bound: false,
                    argc: None,
                    arg_types: None,
                    enclosing: Some(Encloser {
                        path,
                        kind: DefKind::Alias,
                    }),
                    span,
                });
            }
            _ => export_reference(ctx, node, specifier, space),
        }
        return;
    }

    // B5: `export * from './x'` — every name except `default`, and the set
    // is a fixed point over the module graph rather than a fact about this
    // file.
    //
    // The star is marked once per declaration space it forwards, because a
    // module's export surface has two halves in TypeScript and `export *`
    // forwards both. Marking only the value half made every type-space name
    // arriving through a barrel report `NoMatchingDefinition`, which claims
    // the export table was complete — and the table is exactly what could not
    // be enumerated. `export type *` forwards the type half alone; the
    // grammar in this build does not parse that form (the keyword lands in an
    // `ERROR` node), so it is recognised from the token text rather than a
    // kind, which is also correct if the grammar later learns it.
    if has_child(node, "*") {
        let type_only = node.children().any(|c| c.text().trim() == "type");
        let spaces: &[DeclSpace] = match (ctx.dialect, type_only) {
            (Dialect::JavaScript, _) => &[DeclSpace::Value],
            (Dialect::TypeScript, true) => &[DeclSpace::Type],
            (Dialect::TypeScript, false) => &[DeclSpace::Value, DeclSpace::Type],
        };
        for forwarded in spaces {
            ctx.push_export(indirect_entry(
                None,
                specifier.clone(),
                ImportedName::All,
                *forwarded,
                span,
            ));
        }
        // One statement names one module, so it is one reference however many
        // spaces it forwards: a second would double-count the export layer.
        export_reference(ctx, node, specifier, spaces[0]);
        return;
    }

    if let Some(clause) = node.children().find(|c| c.kind() == "export_clause") {
        for spec in clause.children().filter(|c| c.kind() == "export_specifier") {
            let space = if has_child(&spec, "type") {
                DeclSpace::Type
            } else {
                space
            };
            let names = specifier_names(&spec);
            let Some(local) = names.first().cloned() else {
                continue;
            };
            let exported = names.get(1).cloned().unwrap_or_else(|| local.clone());
            let entry = match specifier.clone() {
                // B7: a pure indirection — no local binding is created.
                Some(request) => {
                    reexport_reference(ctx, &spec, &request, &local, &exported, space);
                    indirect_entry(
                        Some(exported),
                        Some(request),
                        ImportedName::Named(local),
                        space,
                        span_of(&spec),
                    )
                }
                // B2: the FQN is the *declaring* local name, never the
                // exported one, or renaming an export would re-key an
                // unchanged definition.
                None => local_entry(exported, local, space, span_of(&spec)),
            };
            ctx.push_export(entry);
        }
        return;
    }

    if has_child(node, "default") {
        default_export(ctx, node, span);
        return;
    }

    // `export <declaration>`: the declaration's own rule emits the node, so
    // only the export entries are this arm's.
    if let Some(decl) = node.children().find(|c| c.is_named()) {
        for (name, space) in declared_names(ctx.dialect, &decl) {
            ctx.push_export(local_entry(name.clone(), name, space, span));
        }
    }
}

fn default_export(ctx: &mut Ctx, node: &SgNode, span: Span) {
    let Some(value) = node.children().filter(|c| c.is_named()).last() else {
        return;
    };
    let local = match &*value.kind() {
        // B3: the declaration keeps its own name; the entry is what is
        // called `default`.
        "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "abstract_class_declaration" => class_name(&value),
        "identifier" => Some(value.text().to_string()),
        _ => None,
    };
    let local = match local {
        Some(name) => name,
        None => {
            // B4: `export default <expression>` binds the synthetic
            // `*default*`, which cannot collide with a real identifier.
            let kind = if is_callable_value(&value) {
                DefKind::Function
            } else if is_class_value(&value) {
                DefKind::Type
            } else {
                DefKind::Const
            };
            ctx.push_def(
                kind,
                DEFAULT_LOCAL,
                vec![],
                DeclSpace::Value,
                DefFacets::RUNTIME.union(DefFacets::EXPORTED),
                span_of(&value),
            );
            if value.kind() == "object" {
                // E7: the object's members are nameable through the default
                // import, so they are nodes — but only here, where the node
                // rule says the literal is reachable.
                let _ = object_members(ctx, &value, vec![DEFAULT_LOCAL.to_string()]);
            }
            DEFAULT_LOCAL.to_string()
        }
    };
    ctx.push_export(local_entry(
        "default".to_string(),
        local,
        DeclSpace::Value,
        span,
    ));
}

fn function_declaration(ctx: &mut Ctx, node: &SgNode) {
    let Some(owner) = owner_of(node) else {
        return;
    };
    let Some(name) = name_of(node) else {
        return;
    };
    let space = owner.space.unwrap_or(DeclSpace::Value);
    // C15/F1: a `function_signature` is an ambient declaration or one of an
    // overload set. Either way it is *one* node per name: ECMAScript has no
    // signature-based dispatch, so arity in the FQN would create nodes no
    // reference can name.
    let extra = if node.kind() == "function_signature" {
        DefFacets::ABSTRACT
    } else {
        DefFacets::default()
    };
    let facets = def_facets(space, is_exported(node), space == DeclSpace::Value, extra);
    ctx.push_def(
        DefKind::Function,
        name,
        owner.path,
        space,
        facets,
        span_of(node),
    );
}

fn class_declaration(ctx: &mut Ctx, node: &SgNode) {
    let Some(owner) = owner_of(node) else {
        return;
    };
    let Some(name) = class_name(node) else {
        return;
    };
    let exported = is_exported(node);
    let extra = if node.kind() == "abstract_class_declaration" {
        DefFacets::ABSTRACT
    } else {
        DefFacets::default()
    };
    let span = span_of(node);
    // The constructor exists at runtime; the instance type is erased. Two
    // records, because TypeScript permits `interface C {}` beside
    // `class C {}` and one FQN cannot hold both.
    ctx.push_def(
        DefKind::Type,
        name.clone(),
        owner.path.clone(),
        DeclSpace::Value,
        def_facets(DeclSpace::Value, exported, true, extra),
        span,
    );
    if ctx.dialect == Dialect::TypeScript {
        ctx.push_def(
            DefKind::Type,
            name.clone(),
            owner.path.clone(),
            DeclSpace::Type,
            def_facets(DeclSpace::Type, exported, false, extra),
            span,
        );
    }
    let mut member_owner = owner.path;
    member_owner.push(name);
    if let Some(body) = node.children().find(|c| c.kind() == "class_body") {
        class_members(ctx, &body, &member_owner);
    }
}

/// Whether a class member is reachable from outside its class.
///
/// A `#`-prefixed name is a `PrivateIdentifier`: lexically scoped to the
/// class body, not a property, and impossible to compute. TypeScript's
/// `private`/`protected` are compile-time only, but they still say what the
/// author meant a reference to be allowed to name.
fn member_exported(node: &SgNode, name: &str) -> bool {
    if name.starts_with('#') {
        return false;
    }
    !node
        .children()
        .any(|c| c.kind() == "accessibility_modifier" && c.text() != "public")
}

fn class_members(ctx: &mut Ctx, body: &SgNode, owner: &[String]) {
    for member in body.children() {
        let (kind, extra) = match &*member.kind() {
            "method_definition" => (method_kind(&member), DefFacets::default()),
            "abstract_method_signature" => (DefKind::Method, DefFacets::ABSTRACT),
            "field_definition" | "public_field_definition" => {
                // E5: an arrow-initialised field is named exactly the way a
                // prototype method is — `this.handle`, `inst.handle` — so it
                // is a method for every purpose a reference has.
                let callable = member.field("value").is_some_and(|v| is_callable_value(&v));
                let kind = if callable {
                    DefKind::Method
                } else {
                    DefKind::Field
                };
                let extra = if has_child(&member, "abstract") || has_child(&member, "declare") {
                    DefFacets::ABSTRACT
                } else {
                    DefFacets::default()
                };
                (kind, extra)
            }
            // E4: a static block is a scope, not a definition. E11: a
            // computed name has no static name to give.
            _ => continue,
        };
        // E11 again: `member_name` answers `None` for a computed key rather
        // than inventing a name from the expression text.
        let Some(name) = member_name(&member) else {
            continue;
        };
        let mut extra = extra;
        let mut member_owner = owner.to_vec();
        if has_child(&member, "static") {
            extra = extra.union(DefFacets::STATIC);
        } else {
            // E3/E4/E5: a non-static element is installed on `C.prototype`
            // and a static one on `C` itself, and they are two distinct
            // members that a reference names two different ways —
            // `new C().m()` against `C.m()`. The FQN has to separate them and
            // cannot read `DefFacets::STATIC` to do it: the FQN grammar's
            // fourth invariant forbids reading a facet, because
            // `Encloser::as_definition` zeroes them and an edge out of the
            // method would then start at an identity no node has. So the
            // distinction rides in the owner chain, which `as_definition`
            // preserves, spelled as the property the language actually puts
            // the member on.
            member_owner.push(PROTOTYPE.to_string());
        }
        let exported = member_exported(&member, &name);
        ctx.push_def(
            kind,
            name,
            member_owner,
            DeclSpace::Value,
            def_facets(DeclSpace::Value, exported, true, extra),
            span_of(&member),
        );
    }
}

fn binding_declaration(ctx: &mut Ctx, node: &SgNode) {
    // A `var` is var-scoped and a `let`/`const` is block-scoped, so the two
    // ask a different question about the same ancestry.
    let owner = if node.kind() == "variable_declaration" {
        var_owner_of(node)
    } else {
        owner_of(node)
    };
    let Some(owner) = owner else {
        return;
    };
    let is_const = has_child(node, "const");
    let exported = is_exported(node);
    let space = owner.space.unwrap_or(DeclSpace::Value);
    for declarator in node
        .children()
        .filter(|c| c.kind() == "variable_declarator")
    {
        let Some(pattern) = declarator.field("name") else {
            continue;
        };
        let value = declarator.field("value");
        // E1: the initialiser's shape decides the kind. A rule that read
        // only `function_declaration` would miss most of a modern corpus.
        let kind = match &value {
            Some(v) if is_callable_value(v) => DefKind::Function,
            Some(v) if is_class_value(v) => DefKind::Type,
            _ if is_const => DefKind::Const,
            _ => DefKind::Var,
        };
        let span = span_of(&declarator);
        let names = pattern_names(&pattern);
        for name in &names {
            ctx.push_def(
                kind,
                name.clone(),
                owner.path.clone(),
                space,
                def_facets(
                    space,
                    exported,
                    space == DeclSpace::Value,
                    DefFacets::default(),
                ),
                span,
            );
        }
        // A container's members, but only where the node rule reaches them:
        // the initialiser of a *named* binding. Anything else — an options
        // object, a callback's argument — is a value, and values are not
        // nodes.
        let (Some(value), [name]) = (value, names.as_slice()) else {
            continue;
        };
        let mut member_owner = owner.path.clone();
        member_owner.push(name.clone());
        match &*value.kind() {
            "object" => {
                let _ = object_members(ctx, &value, member_owner);
            }
            "class" | "class_expression" => {
                if let Some(body) = value.children().find(|c| c.kind() == "class_body") {
                    class_members(ctx, &body, &member_owner);
                }
            }
            _ => {}
        }
    }
}

/// Descend into an object literal the node rule reaches.
///
/// Returns each member as `(exported name, local path)` plus whether the
/// list is *complete*: a spread copies names only a runtime value knows, and
/// saying so is the difference between an honest gap and a wrong export map.
/// The caller decides whether the pairs are export entries — they are for
/// `module.exports = {…}` and they are not for `export default {…}`, which
/// exports exactly one name.
fn object_members(
    ctx: &mut Ctx,
    object: &SgNode,
    owner: Vec<String>,
) -> (Vec<(String, String)>, bool) {
    let mut members = Vec::new();
    let mut complete = true;
    for member in object.children() {
        let span = span_of(&member);
        match &*member.kind() {
            "method_definition" => {
                let Some(name) = member_name(&member) else {
                    continue;
                };
                ctx.push_def(
                    method_kind(&member),
                    name.clone(),
                    owner.clone(),
                    DeclSpace::Value,
                    DefFacets::RUNTIME.union(DefFacets::EXPORTED),
                    span,
                );
                let local = format!("{}.{name}", owner.join("."));
                members.push((name, local));
            }
            "pair" => {
                let Some(name) = member_name(&member) else {
                    continue;
                };
                let Some(value) = member.field("value") else {
                    continue;
                };
                if value.kind() == "identifier" {
                    // An alias for a binding this file already declares.
                    members.push((name, value.text().to_string()));
                    continue;
                }
                let kind = if is_callable_value(&value) {
                    DefKind::Function
                } else if is_class_value(&value) {
                    DefKind::Type
                } else {
                    DefKind::Field
                };
                ctx.push_def(
                    kind,
                    name.clone(),
                    owner.clone(),
                    DeclSpace::Value,
                    DefFacets::RUNTIME.union(DefFacets::EXPORTED),
                    span,
                );
                let local = format!("{}.{name}", owner.join("."));
                members.push((name, local));
            }
            "shorthand_property_identifier" => {
                let name = member.text().to_string();
                members.push((name.clone(), name));
            }
            "spread_element" => complete = false,
            _ => {}
        }
    }
    (members, complete)
}

fn interface_declaration(ctx: &mut Ctx, node: &SgNode) {
    let Some(owner) = owner_of(node) else {
        return;
    };
    let Some(name) = name_of(node) else {
        return;
    };
    let span = span_of(node);
    // Type space only, and never `RUNTIME`: an interface is erased at emit,
    // so an edge into one is a compile-time dependency and not a call.
    ctx.push_def(
        DefKind::Type,
        name.clone(),
        owner.path.clone(),
        DeclSpace::Type,
        def_facets(
            DeclSpace::Type,
            is_exported(node),
            false,
            DefFacets::INTERFACE,
        ),
        span,
    );
    let mut member_owner = owner.path;
    member_owner.push(name);
    let Some(body) = node.children().find(|c| c.kind() == "interface_body") else {
        return;
    };
    for member in body.children() {
        let kind = match &*member.kind() {
            "method_signature" => DefKind::Method,
            "property_signature" => DefKind::Field,
            _ => continue,
        };
        let Some(name) = member_name(&member) else {
            continue;
        };
        ctx.push_def(
            kind,
            name,
            member_owner.clone(),
            DeclSpace::Type,
            def_facets(DeclSpace::Type, true, false, DefFacets::ABSTRACT),
            span_of(&member),
        );
    }
}

fn type_alias(ctx: &mut Ctx, node: &SgNode) {
    let Some(owner) = owner_of(node) else {
        return;
    };
    let Some(name) = name_of(node) else {
        return;
    };
    ctx.push_def(
        DefKind::Type,
        name,
        owner.path,
        DeclSpace::Type,
        def_facets(
            DeclSpace::Type,
            is_exported(node),
            false,
            DefFacets::default(),
        ),
        span_of(node),
    );
}

fn enum_declaration(ctx: &mut Ctx, node: &SgNode) {
    let Some(owner) = owner_of(node) else {
        return;
    };
    let Some(name) = name_of(node) else {
        return;
    };
    // C10: a `const enum`'s members are inlined at every use site and no
    // runtime object is emitted. The nodes still exist — a reference names
    // them — but they are not part of the call graph.
    let is_const = has_child(node, "const");
    let extra = if is_const {
        DefFacets::ENUM.union(DefFacets::CONST_ENUM)
    } else {
        DefFacets::ENUM
    };
    let exported = is_exported(node);
    let span = span_of(node);
    // C1: an enum declares in the Type space *and* the Value space.
    ctx.push_def(
        DefKind::Type,
        name.clone(),
        owner.path.clone(),
        DeclSpace::Value,
        def_facets(DeclSpace::Value, exported, !is_const, extra),
        span,
    );
    ctx.push_def(
        DefKind::Type,
        name.clone(),
        owner.path.clone(),
        DeclSpace::Type,
        def_facets(DeclSpace::Type, exported, false, extra),
        span,
    );
    let mut member_owner = owner.path;
    member_owner.push(name);
    let Some(body) = node.children().find(|c| c.kind() == "enum_body") else {
        return;
    };
    for member in body.children() {
        let member_span = span_of(&member);
        let name = match &*member.kind() {
            "property_identifier" => member.text().to_string(),
            "enum_assignment" => match member_name(&member) {
                Some(name) => name,
                None => continue,
            },
            _ => continue,
        };
        // C11: an enum member is nameable as a value and as a literal type.
        ctx.push_def(
            DefKind::Const,
            name.clone(),
            member_owner.clone(),
            DeclSpace::Value,
            def_facets(DeclSpace::Value, true, !is_const, extra),
            member_span,
        );
        ctx.push_def(
            DefKind::Const,
            name,
            member_owner.clone(),
            DeclSpace::Type,
            def_facets(DeclSpace::Type, true, false, extra),
            member_span,
        );
    }
}

/// Whether a namespace body declares anything that exists at runtime.
///
/// C13: a namespace holding only type declarations is *uninstantiated* — no
/// value is emitted, so `T.I` in type position resolves and `T` in value
/// position does not exist.
fn namespace_is_instantiated(node: &SgNode) -> bool {
    let Some(body) = node.children().find(|c| c.kind() == "statement_block") else {
        return false;
    };
    body.children().any(|stmt| {
        let stmt = if stmt.kind() == "export_statement" {
            match stmt.children().find(|c| c.is_named()) {
                Some(inner) => inner,
                None => return false,
            }
        } else {
            stmt
        };
        match &*stmt.kind() {
            "lexical_declaration"
            | "variable_declaration"
            | "function_declaration"
            | "generator_function_declaration"
            | "class_declaration"
            | "abstract_class_declaration" => true,
            "enum_declaration" => !has_child(&stmt, "const"),
            "internal_module" => namespace_is_instantiated(&stmt),
            _ => false,
        }
    })
}

fn namespace_declaration(ctx: &mut Ctx, node: &SgNode) {
    let Some(owner) = owner_of(node) else {
        return;
    };
    let segments = namespace_segments(node);
    if segments.is_empty() {
        return;
    }
    let exported = is_exported(node);
    let instantiated = namespace_is_instantiated(node);
    let span = span_of(node);
    // C12: `namespace A.B.C` binds only `A` in the enclosing scope. The
    // intermediate segments are sugar for nesting, and the extractor unfolds
    // them so that `A.B.C.f()` has something to probe at every hop.
    let mut path = owner.path;
    let last = segments.len() - 1;
    for (i, segment) in segments.iter().enumerate() {
        let extra = if i == last {
            DefFacets::default()
        } else {
            DefFacets::SYNTHETIC
        };
        ctx.push_def(
            DefKind::Module,
            segment.clone(),
            path.clone(),
            DeclSpace::Namespace,
            def_facets(DeclSpace::Namespace, exported, false, extra),
            span,
        );
        if instantiated && i == last {
            ctx.push_def(
                DefKind::Module,
                segment.clone(),
                path.clone(),
                DeclSpace::Value,
                def_facets(DeclSpace::Value, exported, true, DefFacets::default()),
                span,
            );
        }
        path.push(segment.clone());
    }
}

/// The specifier a `/// <reference … />` directive names.
///
/// `path=` names a file and `types=` names an `@types` package; both add
/// something to the program, which is what an import does. `lib=` is
/// deliberately not one: it names a compiler library that is in no
/// repository, so there is nothing for a resolver to look for and reporting
/// it unresolved would blame a repository for a file it never had.
fn reference_directive(text: &str) -> Option<String> {
    for attribute in ["path", "types"] {
        for after in text.split(attribute).skip(1) {
            let after = after.trim_start();
            let Some(after) = after.strip_prefix('=') else {
                continue;
            };
            let after = after.trim_start();
            let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let value = after[1..].split(quote).next()?;
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// A18: a triple-slash reference directive is an import, written as a comment.
fn triple_slash(ctx: &mut Ctx, node: &SgNode) {
    let text = node.text();
    let Some(specifier) = reference_directive(&text) else {
        return;
    };
    ctx.push_import(
        node,
        ImportSyntax::Esm,
        Some(specifier.clone()),
        specifier,
        // It binds no local name: the directive brings a file or a package's
        // globals into the program, never a name into this file's scope.
        Vec::new(),
        DeclSpace::Type,
    );
}

fn ambient_declaration(ctx: &mut Ctx, node: &SgNode) {
    // A17: `declare module "foo" { … }` is a module node with no file behind
    // it. Everything else an `ambient_declaration` wraps is matched by its
    // own rule, and `owner_of` walks through this node unchanged.
    let Some(module) = node.children().find(|c| c.kind() == "module") else {
        return;
    };
    let Some(specifier) = ambient_module_name(&module) else {
        return;
    };
    let Some(owner) = owner_of(node) else {
        return;
    };
    ctx.push_def(
        DefKind::Module,
        specifier,
        owner.path,
        DeclSpace::Namespace,
        DefFacets::RUNTIME
            .union(DefFacets::EXPORTED)
            .union(DefFacets::SYNTHETIC),
        span_of(&module),
    );
}

fn call(ctx: &mut Ctx, node: &SgNode) {
    // A decorator's call is emitted once, as the annotation.
    if node.parent().is_some_and(|p| p.kind() == "decorator") {
        return;
    }
    let Some(callee) = node.field("function") else {
        return;
    };
    match &*callee.kind() {
        // F13/F14: `import(…)` is a call in the grammar and a module load in
        // the language.
        "import" => dynamic_import(ctx, node),
        // C1: `require` is a *parameter* of Node's module wrapper, so it is
        // shadowable. Treating a shadowed `require` as an import invents an
        // edge the language would not make — and, worse for the measurement,
        // classifies it `External`, which sits outside *both* terms of the
        // rate.
        "identifier" if callee.text() == "require" && !require_is_shadowed(&callee) => {
            require_call(ctx, node)
        }
        _ => {
            let target = member_target(&callee);
            ctx.push_ref(
                RefKind::Call,
                DeclSpace::Value,
                node,
                callee.text().to_string(),
                target,
                argument_count(node),
            );
        }
    }
}

/// A dotted property read outside call/new position.
///
/// One chain is one reference. The rule matches both `a.b` and the outer
/// `a.b.c`, but only the outermost selector names the value the expression
/// reads. A member used as a callee or constructor already has the more
/// precise `Call`/`New` kind, so emitting it here too would double-count one
/// site.
///
/// A plain assignment target (`a.b = x`) writes the property without reading
/// its old value and is omitted. An augmented assignment (`a.b += x`) does
/// read the old value, so it remains a `FieldAccess`. Computed access is a
/// `subscript_expression`, not a `member_expression`, and never reaches this
/// function: `a[b]` has no static member name to emit. A member in a
/// TypeScript type position is already the more precise `TypeUse` reference
/// and is likewise not duplicated.
fn member_read(ctx: &mut Ctx, node: &SgNode) {
    if in_type_position(node) {
        return;
    }
    let mut current = node.clone();
    while let Some(parent) = current.parent() {
        let is_field = |name: &str| {
            parent
                .field(name)
                .is_some_and(|field| field.range() == current.range())
        };
        if (parent.kind() == "member_expression" && is_field("object"))
            || (parent.kind() == "call_expression" && is_field("function"))
            || (parent.kind() == "new_expression" && is_field("constructor"))
            || (parent.kind() == "assignment_expression" && is_field("left"))
        {
            return;
        }
        if parent.kind() == "parenthesized_expression"
            && parent
                .children()
                .any(|child| child.is_named() && child.range() == current.range())
        {
            current = parent;
            continue;
        }
        break;
    }
    ctx.push_ref(
        RefKind::FieldAccess,
        DeclSpace::Value,
        node,
        node.text().to_string(),
        member_target(node),
        None,
    );
}

/// Whether `require` at this call site is a name the file itself bound rather
/// than Node's module-wrapper parameter.
///
/// Two questions, because the graph's notion of "local" and the language's
/// do not coincide here. [`is_locally_bound`] answers for every enclosing
/// *scope*, and deliberately answers `false` at module level — a top-level
/// declaration is a node. But a module-level `const require = …` shadows the
/// wrapper's parameter exactly as a function-local one does, and in an ES
/// module there is no wrapper parameter to shadow in the first place.
///
/// C11 is the exception, and it is the reason this is not simply "bound at
/// module level": `const require = createRequire(import.meta.url)` *is* a
/// CommonJS `require`, constructed on purpose, and the specifiers it names
/// are real module edges. Treating that one as a shadow would trade a false
/// `External` for a lost edge.
fn require_is_shadowed(callee: &SgNode) -> bool {
    if is_locally_bound(callee, "require", DeclSpace::Value) {
        return true;
    }
    let Some(program) = callee.ancestors().find(|a| a.kind() == "program") else {
        return false;
    };
    module_scope_binds(&program, "require") && !binds_create_require(&program)
}

/// Whether a subtree declares `require` from `createRequire(…)` — C11.
fn binds_create_require(node: &SgNode) -> bool {
    if node.kind() == "variable_declarator"
        && node.field("name").is_some_and(|n| n.text() == "require")
    {
        return node.field("value").is_some_and(|v| {
            v.kind() == "call_expression"
                && v.field("function").is_some_and(|f| {
                    let text = f.text();
                    text == "createRequire" || text.ends_with(".createRequire")
                })
        });
    }
    node.children().any(|c| binds_create_require(&c))
}

/// The single named argument of a call, when it has exactly one.
fn only_argument<'a>(node: &SgNode<'a>) -> Option<SgNode<'a>> {
    let list = node.field("arguments")?;
    let mut args = list
        .children()
        .filter(|c| c.is_named() && c.kind() != "comment");
    let first = args.next()?;
    args.next().is_none().then_some(first)
}

fn dynamic_import(ctx: &mut Ctx, node: &SgNode) {
    let argument = only_argument(node);
    let specifier = argument.as_ref().and_then(literal_text);
    let raw = specifier
        .clone()
        .or_else(|| argument.as_ref().map(|a| a.text().to_string()))
        .unwrap_or_default();
    // A23: in a type position this is TypeScript's import-type node, which
    // is erased at emit — a different edge from a runtime module load.
    let type_position = in_type_position(node);
    let (syntax, space) = if type_position {
        (ImportSyntax::ImportType, DeclSpace::Type)
    } else {
        (ImportSyntax::DynamicImport, DeclSpace::Value)
    };
    let mut bindings = const_bindings(node, ImportedName::Namespace);
    // A23: `import('./m').Foo` is *both* a module import and a type use, and
    // the two are different work items when either fails. The type use is not
    // an expression: the module is a literal and the member is written down,
    // so reporting `NeedsExpressionType` would claim a type had to be
    // inferred when nothing did.
    //
    // What carries it is a binding, not a new `TargetRoot`: the import site
    // introduces one under a local name spelled `import(<specifier>)`, which
    // no `IdentifierName` can be, and the reference names that binding. The
    // resolver then resolves it by the same path it resolves
    // `import * as ns from './m'; ns.Foo` — one mechanism, and the extractor
    // still states only what this file writes.
    let member = node
        .parent()
        .filter(|p| p.kind() == "member_expression")
        .and_then(|p| p.field("property").map(|prop| (p, prop)));
    let local = format!("import({raw})");
    if type_position && member.is_some() {
        bindings.push(ImportBinding {
            local: local.clone(),
            imported: ImportedName::Namespace,
            space: DeclSpace::Type,
        });
    }
    ctx.push_import(node, syntax, specifier, raw, bindings, space);
    if type_position && let Some((parent, property)) = member {
        ctx.push_ref(
            RefKind::TypeUse,
            DeclSpace::Type,
            &parent,
            parent.text().to_string(),
            RefTarget {
                root: TargetRoot::Name,
                segments: vec![local, property.text().to_string()],
            },
            None,
        );
    }
}

fn require_call(ctx: &mut Ctx, node: &SgNode) {
    ctx.cjs_syntax = true;
    let argument = only_argument(node);
    // C8: `require(pluginName)` and `require('./rules/' + name)` are real
    // module edges with no statically known target. Recording the site with
    // no specifier is the honest answer; guessing one is not.
    let specifier = argument.as_ref().and_then(literal_text);
    let raw = specifier
        .clone()
        .or_else(|| argument.as_ref().map(|a| a.text().to_string()))
        .unwrap_or_default();
    let bindings = require_bindings(node);
    ctx.push_import(
        node,
        ImportSyntax::Require,
        specifier,
        raw,
        bindings,
        DeclSpace::Value,
    );
}

/// The `const` declarator a value expression initialises, walking through the
/// wrappers that do not change the value.
///
/// C6: only `const` is sound. A `let`/`var` alias can be reassigned —
/// `let impl; try { impl = require('./native') } catch { impl = require('./js') }`
/// — and binding it would state a fact the code contradicts.
fn const_declarator<'a>(node: &SgNode<'a>) -> Option<SgNode<'a>> {
    let mut current = node.clone();
    loop {
        let parent = current.parent()?;
        match &*parent.kind() {
            "await_expression"
            | "parenthesized_expression"
            | "non_null_expression"
            | "as_expression" => current = parent,
            "variable_declarator" => {
                let declaration = parent.parent()?;
                return has_child(&declaration, "const").then_some(parent);
            }
            _ => return None,
        }
    }
}

/// The binding a `const x = <expr>` declarator makes, if it makes a simple
/// one.
fn const_bindings(node: &SgNode, imported: ImportedName) -> Vec<ImportBinding> {
    let Some(declarator) = const_declarator(node) else {
        return Vec::new();
    };
    let Some(pattern) = declarator.field("name") else {
        return Vec::new();
    };
    if pattern.kind() != "identifier" {
        return Vec::new();
    }
    vec![ImportBinding {
        local: pattern.text().to_string(),
        imported,
        space: DeclSpace::Value,
    }]
}

/// The local names a `require(…)` call binds — C2's four shapes.
fn require_bindings(call: &SgNode) -> Vec<ImportBinding> {
    // `const a = require('./m').a` binds one exported name.
    if let Some(parent) = call.parent()
        && parent.kind() == "member_expression"
        && parent
            .field("object")
            .is_some_and(|o| o.range() == call.range())
        && let Some(property) = parent.field("property")
    {
        return const_bindings(&parent, ImportedName::Named(property.text().to_string()));
    }
    let Some(declarator) = const_declarator(call) else {
        // Shape 4: `require('./side-effect.js')` binds nothing and is still
        // a module edge.
        return Vec::new();
    };
    let Some(pattern) = declarator.field("name") else {
        return Vec::new();
    };
    match &*pattern.kind() {
        "identifier" => vec![ImportBinding {
            local: pattern.text().to_string(),
            imported: ImportedName::Whole,
            space: DeclSpace::Value,
        }],
        // Shape 2: destructuring is an ordinary assignment in the language
        // and an exported-name list here.
        "object_pattern" => destructured_bindings(&pattern),
        _ => Vec::new(),
    }
}

/// The `{ a, b: c }` shape, as exported-name bindings.
fn destructured_bindings(pattern: &SgNode) -> Vec<ImportBinding> {
    let mut bindings = Vec::new();
    for member in pattern.children() {
        match &*member.kind() {
            "shorthand_property_identifier_pattern" => {
                let name = member.text().to_string();
                bindings.push(ImportBinding {
                    local: name.clone(),
                    imported: ImportedName::Named(name),
                    space: DeclSpace::Value,
                });
            }
            "pair_pattern" => {
                let (Some(key), Some(value)) = (member.field("key"), member.field("value")) else {
                    continue;
                };
                if value.kind() != "identifier" {
                    continue;
                }
                bindings.push(ImportBinding {
                    local: value.text().to_string(),
                    imported: ImportedName::Named(key.text().to_string()),
                    space: DeclSpace::Value,
                });
            }
            _ => {}
        }
    }
    bindings
}

fn new_expression(ctx: &mut Ctx, node: &SgNode) {
    let Some(constructor) = node.field("constructor") else {
        return;
    };
    let target = member_target(&constructor);
    ctx.push_ref(
        RefKind::New,
        DeclSpace::Value,
        node,
        constructor.text().to_string(),
        target,
        argument_count(node),
    );
}

fn jsx_element(ctx: &mut Ctx, node: &SgNode) {
    let Some(name) = node.children().find(|c| {
        matches!(
            &*c.kind(),
            "identifier" | "member_expression" | "nested_identifier"
        )
    }) else {
        return;
    };
    // F11: JSX *is* a call after transformation, so `Call` is the honest
    // classification.
    //
    // C26: an element name that is lowercase **and** undotted is an
    // *intrinsic* — a host element checked against `JSX.IntrinsicElements`,
    // never a binding in scope. That is a lookup in the **Type** space, and
    // saying so is the whole of the fact: `Call` in the Type space is a
    // combination no other site in this extractor produces, so the resolver
    // can act on it without the core growing a `RefKind` this track would
    // have to add to a shared file. The convention itself is a React
    // transform rule (`@babel/plugin-transform-react-jsx`) and not part of
    // the JSX grammar, which is why it is written down here.
    let target = member_target(&name);
    let intrinsic = target.root == TargetRoot::Name
        && target.segments.len() == 1
        && target.segments[0]
            .chars()
            .next()
            .is_some_and(|c| c.is_lowercase());
    let space = if intrinsic {
        DeclSpace::Type
    } else {
        DeclSpace::Value
    };
    ctx.push_ref(
        RefKind::Call,
        space,
        &name,
        name.text().to_string(),
        target,
        None,
    );
}

fn heritage(ctx: &mut Ctx, node: &SgNode) {
    let space = match &*node.kind() {
        // F9: `extends` on a class names a *value* — the constructor.
        "class_heritage" | "extends_clause" => DeclSpace::Value,
        // C29: `implements` and an interface's `extends` name types.
        _ => DeclSpace::Type,
    };
    for child in node.children().filter(|c| c.is_named()) {
        match &*child.kind() {
            // TypeScript nests the clauses one level deeper; each is matched
            // in its own right, so this arm must not emit them twice.
            "extends_clause" | "implements_clause" | "type_arguments" => continue,
            "identifier"
            | "member_expression"
            | "nested_identifier"
            | "type_identifier"
            | "nested_type_identifier"
            | "generic_type" => {
                let inner = if child.kind() == "generic_type" {
                    match child.field("name") {
                        Some(name) => name,
                        None => continue,
                    }
                } else {
                    child
                };
                let target = member_target(&inner);
                ctx.push_ref(
                    RefKind::Inherit,
                    space,
                    &inner,
                    inner.text().to_string(),
                    target,
                    None,
                );
            }
            // F9 again: `extends mixin(Base)` is a call, and the call rule
            // has it. Nothing here is resolvable.
            _ => {}
        }
    }
}

fn type_use(ctx: &mut Ctx, node: &SgNode) {
    let Some(parent) = node.parent() else {
        return;
    };
    match &*parent.kind() {
        // The tail of `N.T`; the whole name is emitted once.
        "nested_type_identifier" => return,
        // A type parameter's own name is a local binding, not a reference.
        "type_parameter" => return,
        // Heritage is `Inherit`; conflating the two inflates the call graph.
        "implements_clause" | "extends_type_clause" | "extends_clause" | "class_heritage" => return,
        // A declaration's own name is not a reference to it — but only the
        // `name` field is the declaration's name. `type A = B` writes both
        // in the same node, and dropping `B` would delete a real reference.
        "interface_declaration"
        | "type_alias_declaration"
        | "class_declaration"
        | "abstract_class_declaration"
        | "enum_declaration"
        | "internal_module"
            if parent
                .field("name")
                .is_some_and(|n| n.range() == node.range()) =>
        {
            return;
        }
        _ => {}
    }
    let target = member_target(node);
    ctx.push_ref(
        RefKind::TypeUse,
        DeclSpace::Type,
        node,
        node.text().to_string(),
        target,
        None,
    );
}

fn type_query(ctx: &mut Ctx, node: &SgNode) {
    let Some(inner) = node.children().find(|c| {
        matches!(
            &*c.kind(),
            "identifier" | "nested_identifier" | "member_expression"
        )
    }) else {
        return;
    };
    // C20: a type-position reference into the **Value** space. This is why
    // `RefKind` alone cannot select the table a reference consults.
    let target = member_target(&inner);
    ctx.push_ref(
        RefKind::TypeUse,
        DeclSpace::Value,
        &inner,
        inner.text().to_string(),
        target,
        None,
    );
}

fn decorator(ctx: &mut Ctx, node: &SgNode) {
    let Some(inner) = node.children().find(|c| c.is_named()) else {
        return;
    };
    let (target_node, argc) = if inner.kind() == "call_expression" {
        match inner.field("function") {
            Some(callee) => (callee, argument_count(&inner)),
            None => return,
        }
    } else {
        (inner, None)
    };
    let target = member_target(&target_node);
    ctx.push_ref(
        RefKind::Annotation,
        DeclSpace::Value,
        &target_node,
        target_node.text().to_string(),
        target,
        argc,
    );
}

/// Definitions and exports written as assignments — the CommonJS surface,
/// plus the ES5 prototype idiom that predates `class`.
fn assignment(ctx: &mut Ctx, node: &SgNode) {
    let Some(owner) = owner_of(node) else {
        return;
    };
    let (Some(left), Some(right)) = (node.field("left"), node.field("right")) else {
        return;
    };
    let target = member_target(&left);
    let span = span_of(node);
    let segments: Vec<&str> = target.segments.iter().map(String::as_str).collect();
    match (&target.root, segments.as_slice()) {
        // C3: whole-object assignment sets the module's public surface.
        (TargetRoot::Name, ["module", "exports"]) => {
            cjs_default_export(ctx, &right, &owner.path, span)
        }
        // C4: `exports.foo` and `module.exports.foo` name one export each.
        (TargetRoot::Name, ["exports", name] | ["module", "exports", name]) => {
            cjs_named_export(ctx, name, &right, &owner.path, span)
        }
        // C5: the module wrapper is called with `this === module.exports`,
        // so a CommonJS file's top-level `this.x = …` is an export. In an
        // ES module it is a `TypeError`, which is why the kind gates it.
        (TargetRoot::This { .. }, [name]) if owner.path.is_empty() && !ctx.esm_syntax => {
            cjs_named_export(ctx, name, &right, &owner.path, span)
        }
        // E6: `C.prototype.m = function(){}`. The FQN is deliberately the
        // same one `class C { m(){} }` produces, so one scheme covers both
        // eras of the language.
        (TargetRoot::Name, [class, "prototype", name]) => {
            let kind = if is_callable_value(&right) {
                DefKind::Method
            } else {
                DefKind::Field
            };
            ctx.push_def(
                kind,
                *name,
                vec![(*class).to_string(), PROTOTYPE.to_string()],
                DeclSpace::Value,
                DefFacets::RUNTIME.union(DefFacets::EXPORTED),
                span,
            );
        }
        _ => {}
    }
}

/// `module.exports = <value>`.
fn cjs_default_export(ctx: &mut Ctx, value: &SgNode, owner: &[String], span: Span) {
    ctx.cjs_syntax = true;
    let mut member_owner = owner.to_vec();
    member_owner.push(DEFAULT_LOCAL.to_string());
    let local = match &*value.kind() {
        "object" => {
            ctx.push_def(
                DefKind::Const,
                DEFAULT_LOCAL,
                owner.to_vec(),
                DeclSpace::Value,
                DefFacets::RUNTIME.union(DefFacets::EXPORTED),
                span,
            );
            let (members, complete) = object_members(ctx, value, member_owner);
            for (name, local) in members {
                ctx.push_export(local_entry(name, local, DeclSpace::Value, span));
            }
            if !complete {
                // C9: a spread copies names only a runtime value knows, so
                // the export set is not enumerable. Saying nothing here
                // would let a later name lookup report "absent" for a name
                // that may well be present.
                ctx.push_export(ExportEntry {
                    export_name: None,
                    local_name: None,
                    module_request: None,
                    import_name: None,
                    space: DeclSpace::Value,
                    span,
                });
            }
            Some(DEFAULT_LOCAL.to_string())
        }
        _ if is_callable_value(value) || is_class_value(value) => {
            let name = class_name(value);
            match name {
                // A named function expression's name is not a module-level
                // binding, so the node is the synthetic one either way; the
                // name is kept because it is what a stack trace shows.
                Some(name) => {
                    let kind = if is_class_value(value) {
                        DefKind::Type
                    } else {
                        DefKind::Function
                    };
                    ctx.push_def(
                        kind,
                        name.clone(),
                        owner.to_vec(),
                        DeclSpace::Value,
                        DefFacets::RUNTIME.union(DefFacets::EXPORTED),
                        span_of(value),
                    );
                    Some(name)
                }
                None => {
                    ctx.push_def(
                        DefKind::Function,
                        DEFAULT_LOCAL,
                        owner.to_vec(),
                        DeclSpace::Value,
                        DefFacets::RUNTIME.union(DefFacets::EXPORTED),
                        span_of(value),
                    );
                    Some(DEFAULT_LOCAL.to_string())
                }
            }
        }
        // `module.exports = Parser` re-points the surface at an existing
        // binding and declares nothing new.
        "identifier" => Some(value.text().to_string()),
        // The shape is a call result, a spread, or an import: not knowable
        // without running the module.
        _ => None,
    };
    ctx.push_export(ExportEntry {
        export_name: Some("default".to_string()),
        local_name: local,
        module_request: None,
        import_name: None,
        space: DeclSpace::Value,
        span,
    });
}

/// `exports.foo = <value>` and its two spellings.
fn cjs_named_export(ctx: &mut Ctx, name: &str, value: &SgNode, owner: &[String], span: Span) {
    ctx.cjs_syntax = true;
    let mut member_owner = owner.to_vec();
    member_owner.push(DEFAULT_LOCAL.to_string());
    let local = if value.kind() == "identifier" {
        // An alias for a binding this file already declares.
        value.text().to_string()
    } else {
        let kind = if is_callable_value(value) {
            DefKind::Function
        } else if is_class_value(value) {
            DefKind::Type
        } else {
            DefKind::Field
        };
        ctx.push_def(
            kind,
            name,
            member_owner.clone(),
            DeclSpace::Value,
            DefFacets::RUNTIME.union(DefFacets::EXPORTED),
            span,
        );
        format!("{DEFAULT_LOCAL}.{name}")
    };
    ctx.push_export(local_entry(name.to_string(), local, DeclSpace::Value, span));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn js(source: &str) -> EcmaFacts {
        extract(Dialect::JavaScript, "src/a.js", source)
    }

    fn ts(source: &str) -> EcmaFacts {
        extract(Dialect::TypeScript, "src/a.ts", source)
    }

    /// Every definition with this name, in emission order.
    fn defs<'f>(facts: &'f EcmaFacts, name: &str) -> Vec<&'f Definition> {
        facts.defs.iter().filter(|d| d.name == name).collect()
    }

    /// Every *export alias* with this name — the export-surface entries, not
    /// declarations.
    fn aliases<'f>(facts: &'f EcmaFacts, name: &str) -> Vec<&'f Definition> {
        defs(facts, name)
            .into_iter()
            .filter(|d| d.kind == DefKind::Alias)
            .collect()
    }

    /// The one *declaration* with this name, or a panic naming what was found.
    ///
    /// Export aliases are excluded: `export { p as parse }` puts `parse` in
    /// the node keyspace so an importer can probe it (E13/B10), but nothing in
    /// this file declares anything called `parse`.
    fn one<'f>(facts: &'f EcmaFacts, name: &str) -> &'f Definition {
        let found: Vec<&Definition> = defs(facts, name)
            .into_iter()
            .filter(|d| d.kind != DefKind::Alias)
            .collect();
        assert_eq!(
            found.len(),
            1,
            "expected one `{name}`, found {}: {:?}",
            found.len(),
            facts.defs.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
        found[0]
    }

    fn refs(facts: &EcmaFacts, kind: RefKind) -> Vec<&Reference> {
        facts.refs.iter().filter(|r| r.kind == kind).collect()
    }

    /// The reference whose site text is exactly `raw`.
    fn site<'f>(facts: &'f EcmaFacts, raw: &str) -> &'f Reference {
        facts
            .refs
            .iter()
            .find(|r| r.raw_target == raw)
            .unwrap_or_else(|| panic!("no reference site `{raw}`"))
    }

    /// The export entry for this name.
    fn export<'f>(facts: &'f EcmaFacts, name: &str) -> &'f ExportEntry {
        facts
            .header
            .exports
            .iter()
            .find(|e| e.export_name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no export named `{name}`: {:?}", facts.header.exports))
    }

    fn segments(reference: &Reference) -> Vec<&str> {
        reference
            .target
            .segments
            .iter()
            .map(String::as_str)
            .collect()
    }

    // ---- §2 module identity and module kind -----------------------------

    #[test]
    fn the_file_is_the_module_and_its_name_is_its_path() {
        // A1/E12: there is no directory scope. Two same-named definitions in
        // one directory are two distinct nodes, so the module's identity has
        // to be the path and nothing coarser.
        let facts = js("export function parse(){}\n");
        let module = &facts.defs[0];
        assert_eq!(module.kind, DefKind::Module);
        assert_eq!(module.name, "src/a.js");
        assert_eq!(module.space, DeclSpace::Namespace);
        assert!(module.owner.is_empty());
        // The node exists even when the file says nothing, because
        // `import './side-effect.js'` needs something to hit.
        let empty = js("");
        assert_eq!(empty.defs[0].kind, DefKind::Module);
        assert_eq!(empty.defs[0].name, "src/a.js");
    }

    #[test]
    fn module_kind_comes_from_the_extension_first_then_syntax() {
        // A6, NODE `ESM_FILE_FORMAT`. Candidate generation depends on this:
        // ESM does no extension probing and no index resolution.
        let mjs = extract(Dialect::JavaScript, "a.mjs", "const x = require('y');\n");
        assert_eq!(mjs.header.module_kind, ModuleKind::Esm);
        assert_eq!(mjs.header.module_kind_source, ModuleKindSource::Extension);

        let cjs = extract(Dialect::JavaScript, "a.cjs", "import x from './y.js';\n");
        assert_eq!(cjs.header.module_kind, ModuleKind::CommonJs);
        assert_eq!(cjs.header.module_kind_source, ModuleKindSource::Extension);

        let esm = js("import x from './y.js';\n");
        assert_eq!(esm.header.module_kind, ModuleKind::Esm);
        assert_eq!(esm.header.module_kind_source, ModuleKindSource::Syntax);

        let cjs_syntax = js("module.exports = {};\n");
        assert_eq!(cjs_syntax.header.module_kind, ModuleKind::CommonJs);
        assert_eq!(
            cjs_syntax.header.module_kind_source,
            ModuleKindSource::Syntax
        );

        // Nothing in the file decided it. The nearest `package.json` does,
        // and a guess dressed as a fact is what this variant exists to stop.
        let quiet = js("function f(){}\n");
        assert_eq!(quiet.header.module_kind, ModuleKind::Undecided);
        assert_eq!(quiet.header.module_kind_source, ModuleKindSource::Undecided);
    }

    #[test]
    fn a_file_with_no_import_or_export_is_a_script() {
        // B21: a Script's top-level declarations reach the global scope, and
        // the sloppy-mode hazards apply only there.
        assert!(js("var x = 1;\n").header.script);
        assert!(!js("export const x = 1;\n").header.script);
        assert!(!js("import './x.js';\n").header.script);
        // A dynamic `import()` is legal in CommonJS and says nothing.
        assert!(js("import('./x.js');\n").header.script);
    }

    // ---- §2/§4 the sites that name a module ------------------------------

    #[test]
    fn every_import_form_is_a_reference_and_a_header_entry() {
        // The reference is the extractor's; the binding effect it has is the
        // resolver's. Both halves exist, and the driver resolves only from
        // `refs` — resolving from the header too would count every import
        // twice.
        let facts = js(concat!(
            "import parse from './p.js';\n",
            "import { a, b as c } from './m.js';\n",
            "import * as ns from './n.js';\n",
            "import './side.js';\n",
        ));
        let imports = refs(&facts, RefKind::Import);
        assert_eq!(imports.len(), 4);
        assert_eq!(facts.header.imports.len(), 4);
        for (reference, entry) in imports.iter().zip(&facts.header.imports) {
            assert_eq!(Some(&reference.raw_target), entry.specifier.as_ref());
            assert_eq!(reference.span, entry.span);
            assert_eq!(reference.target.root, TargetRoot::Name);
        }

        // F2: the local name of a default import is unrelated to the
        // definition's name, so the binding is the only way back.
        assert_eq!(
            facts.header.imports[0].bindings,
            [ImportBinding {
                local: "parse".into(),
                imported: ImportedName::Default,
                space: DeclSpace::Value,
            }]
        );
        assert_eq!(
            facts.header.imports[1].bindings,
            [
                ImportBinding {
                    local: "a".into(),
                    imported: ImportedName::Named("a".into()),
                    space: DeclSpace::Value,
                },
                ImportBinding {
                    local: "c".into(),
                    imported: ImportedName::Named("b".into()),
                    space: DeclSpace::Value,
                },
            ]
        );
        // F4: a namespace binding, which `util.parse()` resolves through.
        assert_eq!(
            facts.header.imports[2].bindings,
            [ImportBinding {
                local: "ns".into(),
                imported: ImportedName::Namespace,
                space: DeclSpace::Value,
            }]
        );
        // B15: a side-effect import names no symbol and is still an edge.
        assert!(facts.header.imports[3].bindings.is_empty());
    }

    #[test]
    fn a_type_only_import_lands_in_the_type_space() {
        // C17: `import type` is fully elided from the emitted JavaScript, so
        // the edge it makes is a compile-time dependency and not a call.
        let facts = ts("import type { T } from './t';\nimport { type U, v } from './t';\n");
        let imports = refs(&facts, RefKind::Import);
        assert_eq!(imports[0].space, DeclSpace::Type);
        assert_eq!(imports[1].space, DeclSpace::Value);
        let inline = &facts.header.imports[1].bindings;
        assert_eq!(inline[0].local, "U");
        assert_eq!(inline[0].space, DeclSpace::Type);
        assert_eq!(inline[1].local, "v");
        assert_eq!(inline[1].space, DeclSpace::Value);
    }

    #[test]
    fn import_equals_require_binds_the_whole_module() {
        // A22. It interacts with `export =`, which is why the imported name
        // is `Whole` rather than `Default`.
        let facts = ts("import x = require('m');\n");
        assert_eq!(facts.header.imports[0].syntax, ImportSyntax::ImportEquals);
        assert_eq!(facts.header.imports[0].specifier.as_deref(), Some("m"));
        assert_eq!(
            facts.header.imports[0].bindings,
            [ImportBinding {
                local: "x".into(),
                imported: ImportedName::Whole,
                space: DeclSpace::Value,
            }]
        );
    }

    #[test]
    fn require_is_an_import_in_all_four_binding_shapes() {
        // C2. The binding table is populated from *expression* forms, which
        // is the single biggest extractor difference from Go.
        let facts = js(concat!(
            "const m = require('./m.js');\n",
            "const { d, e: ee } = require('./m.js');\n",
            "const single = require('./m.js').only;\n",
            "require('./side.js');\n",
        ));
        let imports = refs(&facts, RefKind::Import);
        assert_eq!(imports.len(), 4);
        assert!(
            facts
                .header
                .imports
                .iter()
                .all(|i| i.syntax == ImportSyntax::Require)
        );
        assert_eq!(
            facts.header.imports[0].bindings,
            [ImportBinding {
                local: "m".into(),
                imported: ImportedName::Whole,
                space: DeclSpace::Value,
            }]
        );
        assert_eq!(
            facts.header.imports[1].bindings,
            [
                ImportBinding {
                    local: "d".into(),
                    imported: ImportedName::Named("d".into()),
                    space: DeclSpace::Value,
                },
                ImportBinding {
                    local: "ee".into(),
                    imported: ImportedName::Named("e".into()),
                    space: DeclSpace::Value,
                },
            ]
        );
        assert_eq!(
            facts.header.imports[2].bindings,
            [ImportBinding {
                local: "single".into(),
                imported: ImportedName::Named("only".into()),
                space: DeclSpace::Value,
            }]
        );
        assert!(facts.header.imports[3].bindings.is_empty());
    }

    #[test]
    fn a_shadowed_require_is_a_call_and_not_an_import() {
        // C1: `require` is a parameter of Node's module wrapper, therefore
        // shadowable. Treating a shadowed one as an import invents an edge
        // the language would not make.
        let facts = js("function f(require){ require('./nope.js'); }\n");
        assert!(refs(&facts, RefKind::Import).is_empty());
        let call = site(&facts, "require");
        assert_eq!(call.kind, RefKind::Call);
        assert!(call.locally_bound);
    }

    #[test]
    fn a_module_level_require_declaration_shadows_the_wrapper_too() {
        // The same case one scope out, and the one that matters more: a
        // module-level shadow made the call `External("node:fs")`, which sits
        // outside *both* terms of the rate, so a wrong classification here
        // raised the rate by deleting a reference.
        let facts = js("const require = () => {};\nrequire('fs');\n");
        assert!(refs(&facts, RefKind::Import).is_empty());
        let call = site(&facts, "require");
        assert_eq!(call.kind, RefKind::Call);
        assert!(
            !call.locally_bound,
            "a module-level declaration is a node, not a local",
        );
        // Every shape of module-level binding, including one a `var` hoists
        // out of a block.
        for src in [
            "function require(){}\nrequire('fs');\n",
            "var require = 1;\nrequire('fs');\n",
            "{ var require = 1; }\nrequire('fs');\n",
        ] {
            assert!(
                refs(&js(src), RefKind::Import).is_empty(),
                "still an import in:\n{src}",
            );
        }
    }

    #[test]
    fn a_created_require_is_still_require() {
        // C11: `const require = createRequire(import.meta.url)` *is* a
        // CommonJS `require`, so the specifiers it names are real module
        // edges. The module-level shadow check must not swallow it.
        let facts = js(concat!(
            "import { createRequire } from 'node:module';\n",
            "const require = createRequire(import.meta.url);\n",
            "require('./m.js');\n",
        ));
        assert!(
            facts
                .header
                .imports
                .iter()
                .any(|i| i.specifier.as_deref() == Some("./m.js")),
        );
    }

    #[test]
    fn a_module_level_var_in_a_block_is_a_node() {
        // D3: `var` is `VarScopedDeclarations`, so it belongs to the nearest
        // function or module environment and not to the block it is written
        // in. Treating the block as its scope emitted no definition at all,
        // and the call inside it became `LocalBinding` while the call after
        // it became `NoMatchingDefinition` — one binding, two wrong answers.
        let facts = js("if (x) { var f = () => {}; }\nf();\n");
        assert_eq!(one(&facts, "f").kind, DefKind::Function);
        assert!(one(&facts, "f").owner.is_empty());
        assert!(!site(&facts, "f").locally_bound);
        // A `let` in the same place is still a local, and still not a node.
        let lexical = js("if (x) { let g = () => {}; g(); }\n");
        assert!(defs(&lexical, "g").is_empty());
        assert!(site(&lexical, "g").locally_bound);
        // And inside a function a `var` is a local wherever it is written.
        let inner = js("function outer(){ if (x) { var h = () => {}; } h(); }\n");
        assert!(defs(&inner, "h").is_empty());
        assert!(site(&inner, "h").locally_bound);
    }

    #[test]
    fn a_triple_slash_directive_is_an_import() {
        // A18: an import that is not an import statement. Emitting nothing
        // for it is not an honest reason — it is a reference that never
        // reaches a bucket at all.
        let facts = ts(concat!(
            "/// <reference path=\"./types.d.ts\" />\n",
            "/// <reference types=\"node\" />\n",
            "/// <reference lib=\"dom\" />\n",
            "let x: Foo;\n",
        ));
        let specifiers: Vec<Option<&str>> = facts
            .header
            .imports
            .iter()
            .map(|i| i.specifier.as_deref())
            .collect();
        assert_eq!(specifiers, [Some("./types.d.ts"), Some("node")]);
        // `lib=` names a compiler library that is in no repository, so there
        // is nothing to look for and nothing is claimed.
        assert_eq!(refs(&facts, RefKind::Import).len(), 2);
        assert!(facts.header.imports.iter().all(|i| i.bindings.is_empty()));
        // A directive does not make the file a module.
        assert!(facts.header.script);
    }

    #[test]
    fn a_lowercase_jsx_element_is_an_intrinsic_and_a_capitalised_one_is_a_binding() {
        // C26/F11: `<div/>` is a host element checked against
        // `JSX.IntrinsicElements` — a Type-space lookup, and never a binding
        // in this repository. `<Button/>` and `<Icons.Star/>` are value
        // references. The convention is React's transform rule, not the JSX
        // grammar's, which is why it is written down.
        let facts = js("const a = <div />;\nconst b = <Button />;\nconst c = <Icons.Star />;\n");
        let by = |raw: &str| {
            refs(&facts, RefKind::Call)
                .into_iter()
                .find(|r| r.raw_target == raw)
                .unwrap_or_else(|| panic!("no `{raw}` element"))
                .clone()
        };
        assert_eq!(by("div").space, DeclSpace::Type);
        assert_eq!(by("Button").space, DeclSpace::Value);
        assert_eq!(by("Icons.Star").space, DeclSpace::Value);
    }

    #[test]
    fn only_a_const_require_alias_binds() {
        // C6/C7: `let`/`var` aliases are mutable, and
        // `try { impl = require('./native') } catch { impl = require('./js') }`
        // makes both specifiers real edges with no sound binding. Both edges
        // are emitted; neither binds.
        let facts = js(concat!(
            "let impl;\n",
            "try { impl = require('./native.js'); } catch { impl = require('./js.js'); }\n",
            "var loose = require('./z.js');\n",
        ));
        assert_eq!(refs(&facts, RefKind::Import).len(), 3);
        assert!(
            facts.header.imports.iter().all(|i| i.bindings.is_empty()),
            "a reassignable alias binds nothing: {:?}",
            facts.header.imports
        );
    }

    #[test]
    fn a_computed_specifier_is_recorded_without_one() {
        // C8/F14: extremely common in plugin architectures. Recording the
        // site with no specifier is the honest answer; guessing one is the
        // failure mode this project exists to avoid.
        let facts = js(concat!(
            "require(pluginName);\n",
            "require('./rules/' + name);\n",
            "import(`./locales/${lang}.js`);\n",
        ));
        assert_eq!(refs(&facts, RefKind::Import).len(), 3);
        for entry in &facts.header.imports {
            assert_eq!(entry.specifier, None);
            assert!(!entry.raw_specifier.is_empty());
        }
    }

    #[test]
    fn a_literal_dynamic_import_is_a_module_edge_and_a_namespace_binding() {
        // F13.
        let facts = js("const m = await import('./x.js');\n");
        let entry = &facts.header.imports[0];
        assert_eq!(entry.syntax, ImportSyntax::DynamicImport);
        assert_eq!(entry.specifier.as_deref(), Some("./x.js"));
        assert_eq!(
            entry.bindings,
            [ImportBinding {
                local: "m".into(),
                imported: ImportedName::Namespace,
                space: DeclSpace::Value,
            }]
        );
        // A template with no substitution names exactly one module.
        let template = js("import(`./x.js`);\n");
        assert_eq!(
            template.header.imports[0].specifier.as_deref(),
            Some("./x.js")
        );
    }

    #[test]
    fn an_import_type_node_is_both_an_import_and_a_type_use() {
        // A23: one written form, two references, and they fail differently.
        let facts = ts("type Imported = import('./m').Foo;\n");
        let import = &facts.header.imports[0];
        assert_eq!(import.syntax, ImportSyntax::ImportType);
        assert_eq!(import.specifier.as_deref(), Some("./m"));
        assert_eq!(refs(&facts, RefKind::Import)[0].space, DeclSpace::Type);
        let type_use = refs(&facts, RefKind::TypeUse);
        assert_eq!(type_use.len(), 1);
        assert_eq!(type_use[0].raw_target, "import('./m').Foo");
        // The module is a literal and the member is written down, so this is
        // a *named* target — not an expression whose type has to be inferred.
        // The root names the binding the import site introduced, which no
        // `IdentifierName` can spell, so nothing else can name it by accident.
        assert_eq!(type_use[0].target.root, TargetRoot::Name);
        assert_eq!(
            type_use[0].target.segments,
            ["import(./m)".to_string(), "Foo".to_string()]
        );
        assert!(
            import
                .bindings
                .iter()
                .any(|b| b.local == "import(./m)" && b.space == DeclSpace::Type),
        );
    }

    // ---- §3 exports, re-exports, and the export map ---------------------

    #[test]
    fn a_named_export_of_a_declaration_is_an_entry_beside_the_node() {
        // B1: the definition needs no field change; the export fact is a
        // separate record.
        let facts = js("export function parse(){}\nexport const X = 1;\n");
        assert!(one(&facts, "parse").facets.contains(DefFacets::EXPORTED));
        let entry = export(&facts, "parse");
        assert_eq!(entry.local_name.as_deref(), Some("parse"));
        assert_eq!(entry.module_request, None);
        assert_eq!(export(&facts, "X").local_name.as_deref(), Some("X"));
    }

    #[test]
    fn a_renamed_export_keeps_the_declaring_local_name() {
        // B2: the FQN must be the declaring local name, never the exported
        // one — renaming an export would otherwise re-key an unchanged
        // definition and cascade a re-resolve.
        let facts = js("function p(){}\nexport { p as parse };\n");
        assert_eq!(one(&facts, "p").name, "p");
        assert_eq!(one(&facts, "p").kind, DefKind::Function);
        // Nothing here *declares* `parse` — but the export surface carries it,
        // as an alias, so an importer has an identity to probe (E13).
        assert_eq!(aliases(&facts, "parse").len(), 1);
        assert!(aliases(&facts, "parse")[0].owner.is_empty());
        assert_eq!(export(&facts, "parse").local_name.as_deref(), Some("p"));
    }

    #[test]
    fn a_named_default_export_keeps_its_own_name() {
        // B3.
        let facts = js("export default function parse(){}\n");
        assert_eq!(one(&facts, "parse").kind, DefKind::Function);
        assert_eq!(
            export(&facts, "default").local_name.as_deref(),
            Some("parse")
        );
    }

    #[test]
    fn an_anonymous_default_export_binds_the_synthetic_name() {
        // B4: `*default*` is ideal precisely because it is not a valid
        // `IdentifierName`, so no real declaration can collide with it.
        for (source, kind) in [
            ("export default () => {};\n", DefKind::Function),
            ("export default 42;\n", DefKind::Const),
        ] {
            let facts = js(source);
            assert_eq!(one(&facts, DEFAULT_LOCAL).kind, kind, "for `{source}`");
            assert_eq!(
                export(&facts, "default").local_name.as_deref(),
                Some(DEFAULT_LOCAL)
            );
        }
        // E7: an object literal reached through the default import makes its
        // members nameable, so they are nodes — and only here.
        let facts = js("export default { parse(){}, format: () => {} };\n");
        assert_eq!(one(&facts, "parse").owner, [DEFAULT_LOCAL]);
        assert_eq!(one(&facts, "format").kind, DefKind::Function);
        // …and `export default {…}` exports exactly one name.
        assert_eq!(facts.header.exports.len(), 1);
    }

    #[test]
    fn an_options_object_is_not_a_container() {
        // E7 is the node rule doing real work: descending into every object
        // literal would make every options object in the corpus a container.
        let facts = js("call({ onDone(){}, retries: 3 });\n");
        assert!(defs(&facts, "onDone").is_empty());
        assert!(defs(&facts, "retries").is_empty());
    }

    #[test]
    fn a_star_re_export_is_an_entry_and_a_reference() {
        // B5/B11: the extractor states that this module re-exports whatever
        // `./x.js` exports. Computing the name set is `GetExportedNames`, it
        // recurses into the module graph, and it is the resolver's.
        let facts = js("export * from './x.js';\n");
        let entry = &facts.header.exports[0];
        assert_eq!(entry.export_name, None);
        assert_eq!(entry.import_name, Some(ImportedName::All));
        assert_eq!(entry.module_request.as_deref(), Some("./x.js"));
        // B7/B10: without the reference, a barrel file produces no edges at
        // all and the whole re-export layer is invisible.
        let exports = refs(&facts, RefKind::Export);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].raw_target, "./x.js");
        assert_eq!(segments(exports[0]), ["./x.js"]);
    }

    #[test]
    fn a_bare_star_re_export_forwards_both_declaration_spaces() {
        // B5: `export *` forwards a module's *whole* export surface, and in
        // TypeScript that surface has a type half as well as a value half.
        // Marking the star in the value space alone makes a type-space lookup
        // through a barrel report `NoMatchingDefinition` — a claim that the
        // export table was complete — when the table is precisely the thing
        // that could not be enumerated.
        let star_spaces = |facts: &EcmaFacts| -> Vec<DeclSpace> {
            let mut spaces: Vec<DeclSpace> = facts
                .defs
                .iter()
                .filter(|d| d.kind == DefKind::Alias && d.name == STAR_EXPORT)
                .map(|d| d.space)
                .collect();
            spaces.sort_by_key(|s| format!("{s:?}"));
            spaces
        };
        let ts_star = ts("export * from './x';\n");
        assert_eq!(
            star_spaces(&ts_star),
            [DeclSpace::Type, DeclSpace::Value],
            "a TypeScript barrel forwards types too",
        );
        // One statement is one reference to the module it names, whichever
        // spaces it forwards: the export layer must not double-count.
        assert_eq!(refs(&ts_star, RefKind::Export).len(), 1);
        // JavaScript has no type space, so claiming one would mint a marker
        // no reference can name.
        assert_eq!(
            star_spaces(&js("export * from './x.js';\n")),
            [DeclSpace::Value],
        );
    }

    #[test]
    fn a_namespace_re_export_names_the_namespace_object() {
        // B6: unlike a bare star, the namespace object includes `default`.
        let facts = js("export * as util from './u.js';\n");
        let entry = &facts.header.exports[0];
        assert_eq!(entry.export_name.as_deref(), Some("util"));
        assert_eq!(entry.import_name, Some(ImportedName::Namespace));
        assert_eq!(entry.module_request.as_deref(), Some("./u.js"));
        assert_eq!(refs(&facts, RefKind::Export).len(), 1);
    }

    #[test]
    fn an_indirect_re_export_creates_no_local_binding() {
        // B7: `export { x } from 'm'` is a pure indirection.
        let facts = js("export { q, r as s } from './y.js';\n");
        // Nothing local is declared; the only `q` is the export alias.
        assert_eq!(defs(&facts, "q").len(), 1);
        assert_eq!(aliases(&facts, "q").len(), 1);
        let q = export(&facts, "q");
        assert_eq!(q.local_name, None);
        assert_eq!(q.import_name, Some(ImportedName::Named("q".into())));
        assert_eq!(q.module_request.as_deref(), Some("./y.js"));
        let s = export(&facts, "s");
        assert_eq!(s.import_name, Some(ImportedName::Named("r".into())));
        // One reference per *name*, not one per statement: each is an edge
        // out of a different alias node, and one reference for the statement
        // could only be sourced at the module — leaving the barrel's names
        // unlinked, which is the whole failure B7 describes.
        let exports = refs(&facts, RefKind::Export);
        assert_eq!(exports.len(), 2);
        let sources: Vec<Vec<String>> = exports
            .iter()
            .map(|r| r.enclosing.as_ref().expect("an alias source").path.clone())
            .collect();
        assert_eq!(sources, [vec!["q".to_string()], vec!["s".to_string()]]);
        assert_eq!(exports[0].target.segments, ["./y.js", "q"]);
        assert_eq!(exports[1].target.segments, ["./y.js", "r"]);
    }

    #[test]
    fn an_arbitrary_string_export_name_is_carried_verbatim() {
        // B12: an exported name may be any string, including one with `#`,
        // `.`, or spaces. A silent collision in the FQN builder is exactly
        // what the escaping rule exists to stop, so the raw name must
        // survive extraction.
        let facts = js("const x = 1;\nexport { x as \"my-name\" };\n");
        assert_eq!(export(&facts, "my-name").local_name.as_deref(), Some("x"));
        let imported = js("import { \"my-name\" as y } from './a.js';\n");
        assert_eq!(
            imported.header.imports[0].bindings,
            [ImportBinding {
                local: "y".into(),
                imported: ImportedName::Named("my-name".into()),
                space: DeclSpace::Value,
            }]
        );
    }

    #[test]
    fn a_type_only_export_lands_in_the_type_space() {
        // C18: the alias carries a space.
        let facts =
            ts("type T = string;\ntype W = number;\nexport type { T };\nexport { type W };\n");
        assert_eq!(export(&facts, "T").space, DeclSpace::Type);
        assert_eq!(export(&facts, "W").space, DeclSpace::Type);
    }

    #[test]
    fn export_equals_occupies_its_own_export_name() {
        // B12 (TypeScript): the module's export *is* `Foo`, reachable both
        // as `import x = require(…)` and, under `esModuleInterop`, as a
        // default import. Two names, one target, and the resolver needs both
        // spelled out.
        let facts = ts("class Foo {}\nexport = Foo;\n");
        let entry = export(&facts, EXPORT_EQUALS);
        assert_eq!(entry.local_name.as_deref(), Some("Foo"));
        assert_eq!(entry.import_name, Some(ImportedName::Whole));
    }

    // ---- §4 CommonJS ----------------------------------------------------

    #[test]
    fn whole_object_assignment_is_the_default_export() {
        // C3: canonicalised to the ESM export-map vocabulary, so one
        // resolver reads both eras.
        let object = js("function parse(){}\nmodule.exports = { parse, format(){}, k: 1 };\n");
        assert_eq!(
            export(&object, "default").local_name.as_deref(),
            Some(DEFAULT_LOCAL)
        );
        // A shorthand member aliases the binding this file already declares.
        assert_eq!(
            export(&object, "parse").local_name.as_deref(),
            Some("parse")
        );
        assert_eq!(
            defs(&object, "parse").len(),
            1,
            "no second node for `parse`"
        );
        // An inline member is a new definition, owned by the export object.
        assert_eq!(one(&object, "format").owner, [DEFAULT_LOCAL]);
        assert_eq!(
            export(&object, "format").local_name.as_deref(),
            Some("*default*.format")
        );
        assert_eq!(one(&object, "k").kind, DefKind::Field);

        let named = js("module.exports = function parse(){};\n");
        assert_eq!(
            export(&named, "default").local_name.as_deref(),
            Some("parse")
        );

        let anonymous = js("module.exports = () => {};\n");
        assert_eq!(
            export(&anonymous, "default").local_name.as_deref(),
            Some(DEFAULT_LOCAL)
        );

        let rebind = js("module.exports = Parser;\n");
        assert_eq!(
            export(&rebind, "default").local_name.as_deref(),
            Some("Parser")
        );
        assert!(defs(&rebind, "Parser").is_empty());
    }

    #[test]
    fn an_unknowable_export_shape_records_no_local_name() {
        // C9: when `module.exports` is assigned a call result or an import,
        // the shape is genuinely unknown. Claiming a local would be a lie
        // the resolver could not detect.
        let facts = js("module.exports = require('./other.js');\n");
        assert_eq!(export(&facts, "default").local_name, None);
        // A spread means the name set is not enumerable at all, and the
        // entry with no name at all is how that is said.
        let spread = js("module.exports = { ...base, parse(){} };\n");
        assert!(
            spread
                .header
                .exports
                .iter()
                .any(|e| e.export_name.is_none() && e.module_request.is_none()),
            "a spread must be recorded, not silently dropped: {:?}",
            spread.header.exports
        );
    }

    #[test]
    fn property_assignment_is_a_named_export() {
        // C4. Order matters — `exports.a = 1; module.exports = {}` exports
        // nothing named `a` — so each entry carries its span and the
        // resolver reads statement order from it.
        let facts = js(concat!(
            "exports.foo = function(){};\n",
            "module.exports.qux = 1;\n",
            "exports.bar = localThing;\n",
        ));
        assert_eq!(one(&facts, "foo").owner, [DEFAULT_LOCAL]);
        assert_eq!(one(&facts, "foo").kind, DefKind::Function);
        assert_eq!(
            export(&facts, "foo").local_name.as_deref(),
            Some("*default*.foo")
        );
        assert_eq!(one(&facts, "qux").kind, DefKind::Field);
        // An alias for an existing binding declares nothing new — the only
        // `bar` in the file is the export-surface alias.
        assert_eq!(defs(&facts, "bar").len(), 1);
        assert_eq!(aliases(&facts, "bar").len(), 1);
        assert_eq!(
            export(&facts, "bar").local_name.as_deref(),
            Some("localThing")
        );
        let spans: Vec<u32> = facts.header.exports.iter().map(|e| e.span.line).collect();
        assert_eq!(spans, [1, 2, 3], "statement order is recoverable");
    }

    #[test]
    fn top_level_this_is_an_export_only_in_a_commonjs_file() {
        // C5: the module wrapper is called with `this === module.exports`.
        // In an ES module `[[ThisValue]]` is `undefined` and the same line
        // is a `TypeError`, so the kind has to gate it.
        let commonjs = js("this.parse = function(){};\n");
        assert_eq!(one(&commonjs, "parse").owner, [DEFAULT_LOCAL]);
        let esm = js("export const x = 1;\nthis.parse = function(){};\n");
        assert!(
            defs(&esm, "parse").is_empty(),
            "in an ES module this is not an export"
        );
    }

    // ---- §7 what is a nameable definition -------------------------------

    #[test]
    fn a_binding_is_classified_by_its_initialiser() {
        // E1: arrow-function and function-expression initialisers are the
        // dominant modern form. A rule matching only `function_declaration`
        // would miss most of a modern corpus.
        let facts = js(concat!(
            "const f = () => {};\n",
            "const g = function(){};\n",
            "const C = class {};\n",
            "const CONFIG = 1;\n",
            "let n = 2;\n",
            "var v = 3;\n",
        ));
        assert_eq!(one(&facts, "f").kind, DefKind::Function);
        assert_eq!(one(&facts, "g").kind, DefKind::Function);
        assert_eq!(one(&facts, "C").kind, DefKind::Type);
        assert_eq!(one(&facts, "CONFIG").kind, DefKind::Const);
        assert_eq!(one(&facts, "n").kind, DefKind::Var);
        assert_eq!(one(&facts, "v").kind, DefKind::Var);
    }

    #[test]
    fn a_non_exported_module_level_binding_is_still_a_node() {
        // E2: nothing outside the file can name it, but references inside
        // can, and intra-file edges are what make impact work at symbol
        // granularity.
        let facts = js("function helper(){}\n");
        let helper = one(&facts, "helper");
        assert!(!helper.facets.contains(DefFacets::EXPORTED));
        assert!(helper.facets.contains(DefFacets::RUNTIME));
    }

    #[test]
    fn class_members_are_nodes_owned_by_their_class() {
        // E3/E4/E9/E10/E11. An instance member is installed on
        // `C.prototype` and a static one on `C`, and the owner chain says
        // which — the `STATIC` facet cannot, because the FQN may not read a
        // facet (grammar invariant 4) and two members of one name would then
        // share one identity.
        let facts = js(concat!(
            "class C {\n",
            "  m(){}\n",
            "  static s(){}\n",
            "  #p(){}\n",
            "  get v(){}\n",
            "  set v(x){}\n",
            "  handle = () => {};\n",
            "  count = 1;\n",
            "  static sf = 2;\n",
            "  constructor(){}\n",
            "  [computed](){}\n",
            "  static { init(); }\n",
            "}\n",
        ));
        let member = |name: &str| one(&facts, name);
        assert_eq!(member("m").kind, DefKind::Method);
        assert_eq!(member("m").owner, ["C", PROTOTYPE]);
        assert!(!member("m").facets.contains(DefFacets::STATIC));
        assert_eq!(member("s").owner, ["C"]);
        assert!(member("s").facets.contains(DefFacets::STATIC));
        assert_eq!(member("count").owner, ["C", PROTOTYPE]);
        assert_eq!(member("sf").owner, ["C"]);
        // E10: a private name is not a property and cannot be reached from
        // outside the class body.
        assert!(!member("#p").facets.contains(DefFacets::EXPORTED));
        // E9: an accessor pair reads as a field.
        assert_eq!(defs(&facts, "v").len(), 2);
        assert!(
            defs(&facts, "v")
                .iter()
                .all(|d| d.kind == DefKind::Property)
        );
        // E5: an arrow-initialised field is named exactly as a prototype
        // method is, so it is a method for every purpose a reference has.
        assert_eq!(member("handle").kind, DefKind::Method);
        assert_eq!(member("count").kind, DefKind::Field);
        assert!(member("sf").facets.contains(DefFacets::STATIC));
        assert_eq!(member("constructor").kind, DefKind::Constructor);
        // E11: a computed name has no static name; inventing one from the
        // expression text is what this must not do.
        assert!(defs(&facts, "computed").is_empty());
        // E4: a static block is a scope, not a definition.
        assert_eq!(
            site(&facts, "init")
                .enclosing
                .as_ref()
                .map(|e| e.path.clone()),
            Some(vec!["C".to_string()])
        );
    }

    #[test]
    fn es5_prototype_assignment_produces_the_same_shape_as_a_class() {
        // E6: deliberately identical to `class C { m(){} }`, so one FQN
        // scheme covers both eras. Pervasive in older corpora.
        let facts = js("function C(){}\nC.prototype.m = function(){};\nC.prototype.k = 1;\n");
        assert_eq!(one(&facts, "m").kind, DefKind::Method);
        assert_eq!(one(&facts, "m").owner, ["C", PROTOTYPE]);
        assert_eq!(one(&facts, "k").kind, DefKind::Field);
        // Identical to the `class` form, which is the whole point of E6.
        let modern = js("class C { m(){} }\n");
        assert_eq!(one(&facts, "m").owner, one(&modern, "m").owner);
    }

    #[test]
    fn a_static_and_an_instance_member_of_one_name_are_two_identities() {
        // `class C { m(){} static m(){} }` is legal and is two members:
        // `new C().m()` and `C.m()` cannot reach the same node. Before the
        // prototype segment they shared one FQN and one silently won.
        let facts = js("class C { m(){} static m(){} }\n");
        let both = defs(&facts, "m");
        assert_eq!(both.len(), 2);
        let header = EcmaHeader {
            rel_path: "m.js".into(),
            ..EcmaHeader::default()
        };
        let fqns: Vec<String> = both
            .iter()
            .map(|d| {
                crate::track_ecma::resolve::definition_fqn(&header, d)
                    .expect("nameable")
                    .into_string()
            })
            .collect();
        assert_eq!(fqns, ["m.js#value:C.prototype.m", "m.js#value:C.m"]);
    }

    #[test]
    fn locals_are_not_nodes() {
        // E14. Closure variables, parameters, destructured locals and
        // everything inside a function body.
        let facts = js(concat!(
            "function outer(param){\n",
            "  const inner = () => {};\n",
            "  var v = 1;\n",
            "  class Local {}\n",
            "  function nested(){}\n",
            "  const { picked } = source;\n",
            "}\n",
        ));
        for name in ["param", "inner", "v", "Local", "nested", "picked"] {
            assert!(defs(&facts, name).is_empty(), "`{name}` is a local");
        }
        assert_eq!(one(&facts, "outer").kind, DefKind::Function);
    }

    #[test]
    fn a_module_level_destructuring_binds_every_name_it_declares() {
        // The mirror of the last test: at module level each bound name is a
        // module-level binding, and a module-level binding is a node.
        let facts = js("const { a, b: c } = require('./m.js');\nconst [d] = xs;\n");
        for name in ["a", "c", "d"] {
            assert_eq!(one(&facts, name).kind, DefKind::Const, "for `{name}`");
        }
        assert!(defs(&facts, "b").is_empty(), "a pattern key binds nothing");
    }

    // ---- §4 (TypeScript) declaration spaces -----------------------------

    #[test]
    fn a_declaration_lands_in_every_space_it_creates() {
        // C1: `interface Foo {}` beside `const Foo = 1` is legal and makes
        // two symbols. A single record per name would let one silently
        // overwrite the other in the node table.
        let facts = ts(concat!(
            "class C {}\n",
            "interface I {}\n",
            "type A = string;\n",
            "enum E { X }\n",
            "function f(){}\n",
            "const v = 1;\n",
        ));
        let spaces = |name: &str| {
            let mut spaces: Vec<DeclSpace> = defs(&facts, name).iter().map(|d| d.space).collect();
            spaces.sort();
            spaces
        };
        assert_eq!(spaces("C"), [DeclSpace::Value, DeclSpace::Type]);
        assert_eq!(spaces("I"), [DeclSpace::Type]);
        assert_eq!(spaces("A"), [DeclSpace::Type]);
        assert_eq!(spaces("E"), [DeclSpace::Value, DeclSpace::Type]);
        assert_eq!(spaces("f"), [DeclSpace::Value]);
        assert_eq!(spaces("v"), [DeclSpace::Value]);
        // JavaScript has one space, so a class there gets one record.
        let plain = js("class C {}\n");
        assert_eq!(defs(&plain, "C").len(), 1);
        assert_eq!(defs(&plain, "C")[0].space, DeclSpace::Value);
    }

    #[test]
    fn erased_constructs_are_nodes_without_being_runtime() {
        // The `RUNTIME` facet is what keeps an erased construct out of the
        // call graph while leaving it a node in the type space.
        let facts = ts("interface I {}\ntype A = string;\nclass C {}\n");
        let by_space = |name: &str, space: DeclSpace| {
            *defs(&facts, name)
                .iter()
                .find(|d| d.space == space)
                .unwrap_or_else(|| panic!("no `{name}` in {space:?}"))
        };
        assert!(
            !by_space("I", DeclSpace::Type)
                .facets
                .contains(DefFacets::RUNTIME)
        );
        assert!(
            by_space("I", DeclSpace::Type)
                .facets
                .contains(DefFacets::INTERFACE)
        );
        assert!(
            !by_space("A", DeclSpace::Type)
                .facets
                .contains(DefFacets::RUNTIME)
        );
        // A class's constructor is real; its instance type is not.
        assert!(
            by_space("C", DeclSpace::Value)
                .facets
                .contains(DefFacets::RUNTIME)
        );
        assert!(
            !by_space("C", DeclSpace::Type)
                .facets
                .contains(DefFacets::RUNTIME)
        );
    }

    #[test]
    fn an_enum_member_is_a_node_in_both_spaces() {
        // C10/C11: an enum member is nameable as a value and as a literal
        // type, and a `const enum` is inlined at every use site so no
        // runtime object exists.
        let facts = ts("enum E { A = 1, B }\nconst enum CE { X }\n");
        let a = defs(&facts, "A");
        assert_eq!(a.len(), 2);
        assert!(
            a.iter()
                .all(|d| d.kind == DefKind::Const && d.owner == ["E"])
        );
        assert_eq!(defs(&facts, "B").len(), 2);
        let x = defs(&facts, "X");
        assert!(x.iter().all(|d| d.facets.contains(DefFacets::CONST_ENUM)));
        assert!(
            x.iter().all(|d| !d.facets.contains(DefFacets::RUNTIME)),
            "a const enum emits no runtime object"
        );
    }

    #[test]
    fn a_nested_namespace_path_is_unfolded() {
        // C12: `namespace A.B.C` binds only `A` in the enclosing scope, and
        // the nesting is sugar. Unfolding it is what gives `A.B.C.f()`
        // something to probe at every hop; without it the reference falls to
        // "needs type inference", which would be a false report — no
        // inference is required.
        let facts = ts("namespace A.B.C { export function nf(){} }\n");
        assert_eq!(one(&facts, "nf").owner, ["A", "B", "C"]);
        let a = defs(&facts, "A");
        assert_eq!(a.len(), 1);
        assert!(a[0].facets.contains(DefFacets::SYNTHETIC));
        assert!(a[0].owner.is_empty());
        assert_eq!(defs(&facts, "B")[0].owner, ["A"]);
        let c = defs(&facts, "C");
        assert!(
            c.iter().all(|d| !d.facets.contains(DefFacets::SYNTHETIC)),
            "the innermost segment is written, not synthesised"
        );
    }

    #[test]
    fn an_uninstantiated_namespace_has_no_value_side() {
        // C13: a namespace holding only type declarations emits no value, so
        // `T.I` in type position resolves and `T` in value position does not
        // exist.
        let types_only = ts("namespace T { export interface I {} }\n");
        let spaces: Vec<DeclSpace> = defs(&types_only, "T").iter().map(|d| d.space).collect();
        assert_eq!(spaces, [DeclSpace::Namespace]);

        let instantiated = ts("namespace N { export const q = 1; }\n");
        let mut spaces: Vec<DeclSpace> = defs(&instantiated, "N").iter().map(|d| d.space).collect();
        spaces.sort();
        assert_eq!(spaces, [DeclSpace::Value, DeclSpace::Namespace]);
        assert_eq!(one(&instantiated, "q").owner, ["N"]);
    }

    #[test]
    fn a_namespace_member_carries_whether_it_is_exported() {
        // C14: `N.hidden` must not resolve. Go has no export modifier the
        // resolver ever checks; here candidate generation filters on it.
        let facts = ts("namespace N { const hidden = 1; export const shown = 2; }\n");
        assert!(!one(&facts, "hidden").facets.contains(DefFacets::EXPORTED));
        assert!(one(&facts, "shown").facets.contains(DefFacets::EXPORTED));
    }

    #[test]
    fn an_overload_set_and_an_ambient_declaration_are_one_node_per_name() {
        // F1/F4/C15: a reference names the symbol, not a signature.
        // Selecting the signature needs the argument's type, and an arity
        // component in the FQN would create nodes no reference can name.
        let facts = ts(concat!(
            "declare function df(): void;\n",
            "function ov(a: string): void;\n",
            "function ov(a: number): void;\n",
            "function ov(a: any): void {}\n",
        ));
        assert!(one(&facts, "df").facets.contains(DefFacets::ABSTRACT));
        assert!(one(&facts, "df").params.is_none());
        let ov = defs(&facts, "ov");
        assert_eq!(ov.len(), 3, "one record per written signature");
        assert!(
            ov.iter().all(|d| d.params.is_none()),
            "no arity in the identity: the three share one FQN and merge"
        );
        assert_eq!(
            ov.iter()
                .filter(|d| !d.facets.contains(DefFacets::ABSTRACT))
                .count(),
            1,
            "exactly one implementation signature"
        );
    }

    #[test]
    fn an_ambient_module_is_a_module_node_with_no_file_behind_it() {
        // A17: a real symbol table with no source file. Its members belong
        // to it and not to the file that declares it.
        let facts = ts("declare module \"foo\" { export function bar(): void; }\n");
        let module = one(&facts, "foo");
        assert_eq!(module.kind, DefKind::Module);
        assert_eq!(module.space, DeclSpace::Namespace);
        assert!(module.facets.contains(DefFacets::SYNTHETIC));
        assert_eq!(one(&facts, "bar").owner, ["foo"]);
    }

    #[test]
    fn a_global_augmentation_owns_its_declarations_explicitly() {
        // B19: the declaration lands in the global scope, not in this
        // file's module, so its owner cannot be derived from the path.
        let facts = ts("declare global { interface Window {} }\n");
        assert_eq!(one(&facts, "Window").owner, [GLOBAL_OWNER]);
        assert_eq!(one(&facts, "Window").space, DeclSpace::Type);
    }

    #[test]
    fn class_member_visibility_is_recorded() {
        // C14's class-side twin, and E10's: `private` says what the author
        // meant a reference to be allowed to name, and `#p` makes it
        // enforceable.
        let facts = ts(concat!(
            "class C {\n",
            "  private p = 1;\n",
            "  protected q = 2;\n",
            "  public r = 3;\n",
            "  s = 4;\n",
            "}\n",
        ));
        assert!(!one(&facts, "p").facets.contains(DefFacets::EXPORTED));
        assert!(!one(&facts, "q").facets.contains(DefFacets::EXPORTED));
        assert!(one(&facts, "r").facets.contains(DefFacets::EXPORTED));
        assert!(one(&facts, "s").facets.contains(DefFacets::EXPORTED));
    }

    #[test]
    fn an_interface_member_is_a_node_in_the_type_space() {
        // The owner's space decides the member's, so `interface Foo`'s
        // members stay distinguishable from `const Foo`'s.
        let facts = ts("interface I { a(): void; p: number }\n");
        assert_eq!(one(&facts, "a").kind, DefKind::Method);
        assert_eq!(one(&facts, "a").owner, ["I"]);
        assert_eq!(one(&facts, "a").space, DeclSpace::Type);
        assert!(one(&facts, "a").facets.contains(DefFacets::ABSTRACT));
        assert_eq!(one(&facts, "p").kind, DefKind::Field);
    }

    // ---- §8 reference shapes --------------------------------------------

    #[test]
    fn a_dotted_chain_keeps_every_segment() {
        // F3: `RefTarget` with exactly one qualifier cannot express
        // `ns.sub.parse()`, and collapsing a three-deep chain into a
        // "complex" bucket misreports the reason — which corrupts the
        // histogram the project uses as its instrument.
        let facts = js("parse();\nns.parse();\nns.sub.parse();\n");
        assert_eq!(segments(site(&facts, "parse")), ["parse"]);
        assert_eq!(segments(site(&facts, "ns.parse")), ["ns", "parse"]);
        assert_eq!(
            segments(site(&facts, "ns.sub.parse")),
            ["ns", "sub", "parse"]
        );
    }

    #[test]
    fn a_computed_member_call_has_no_name_to_offer() {
        // F5: the key is a runtime value. The raw text is kept so the
        // resolver can say *which* site it was.
        let facts = js("handlers[type]();\nf().m();\n");
        let computed = site(&facts, "handlers[type]");
        assert_eq!(computed.target.root, TargetRoot::Expr);
        assert!(computed.target.segments.is_empty());
        let chained = site(&facts, "f().m");
        assert_eq!(chained.target.root, TargetRoot::Expr);
        assert_eq!(segments(chained), ["m"]);
    }

    #[test]
    fn dotted_member_reads_have_the_same_cut_in_both_ecma_grammars() {
        let source = concat!(
            "function f(local) {\n",
            "  const a = imported.member;\n",
            "  const b = local.member;\n",
            "  const c = make().member;\n",
            "  local.writeOnly = 1;\n",
            "  local.readWrite += 1;\n",
            "  imported.called();\n",
            "  const d = local['computed'];\n",
            "}\n",
        );
        for facts in [js(source), ts(source)] {
            let fields = refs(&facts, RefKind::FieldAccess);
            let targets: Vec<&str> = fields.iter().map(|r| r.raw_target.as_str()).collect();
            assert_eq!(
                targets,
                [
                    "imported.member",
                    "local.member",
                    "make().member",
                    "local.readWrite",
                ],
            );
            assert!(!site(&facts, "imported.member").locally_bound);
            assert!(site(&facts, "local.member").locally_bound);
            assert_eq!(site(&facts, "make().member").target.root, TargetRoot::Expr,);
            assert_eq!(
                refs(&facts, RefKind::Call)
                    .iter()
                    .filter(|r| r.raw_target == "imported.called")
                    .count(),
                1,
            );
            assert_eq!(
                refs(&facts, RefKind::FieldAccess)
                    .iter()
                    .filter(|r| r.raw_target == "imported.called")
                    .count(),
                0,
                "a member callee keeps its more precise kind",
            );
        }
        let type_query = ts("type T = typeof imported.member;\n");
        assert_eq!(refs(&type_query, RefKind::FieldAccess).len(), 0);
        assert_eq!(
            refs(&type_query, RefKind::TypeUse)
                .iter()
                .filter(|r| r.raw_target == "imported.member")
                .count(),
            1,
            "a member in a type query keeps its more precise kind",
        );
    }

    #[test]
    fn this_and_super_are_their_own_roots() {
        // F6/F7: `this.m()` resolves lexically to the enclosing class and
        // `super.m()` through the heritage, so neither may be flattened into
        // a plain name.
        let facts = js("class D extends B { m(){ this.n(); super.n(); } }\n");
        let this_call = site(&facts, "this.n");
        assert_eq!(
            this_call.target.root,
            TargetRoot::This { qualifier: vec![] }
        );
        assert_eq!(segments(this_call), ["n"]);
        let super_call = site(&facts, "super.n");
        assert_eq!(
            super_call.target.root,
            TargetRoot::Super { qualifier: vec![] }
        );
        // The prototype walk needs the heritage resolved first, so it is a
        // reference in its own right.
        let inherit = refs(&facts, RefKind::Inherit);
        assert_eq!(inherit.len(), 1);
        assert_eq!(inherit[0].raw_target, "B");
        assert_eq!(inherit[0].space, DeclSpace::Value);
    }

    #[test]
    fn a_private_member_call_keeps_its_hash() {
        // E10: the easiest resolution case in the language — a `#` member
        // resolves lexically and unambiguously to the enclosing class.
        let facts = js("class C { #m(){} f(){ this.#m(); } }\n");
        assert_eq!(segments(site(&facts, "this.#m")), ["#m"]);
    }

    #[test]
    fn construction_is_its_own_reference_kind() {
        // F8: `new C()` is a `new_expression`, not a `call_expression`, so a
        // single call rule misses every construction site in the corpus.
        let facts = js("new C();\nnew ns.C(1, 2);\n");
        let created = refs(&facts, RefKind::New);
        assert_eq!(created.len(), 2);
        assert_eq!(segments(created[0]), ["C"]);
        assert_eq!(created[0].argc, Some(0));
        assert_eq!(segments(created[1]), ["ns", "C"]);
        assert_eq!(created[1].argc, Some(2));
    }

    #[test]
    fn heritage_is_an_inherit_reference_in_the_right_space() {
        // F9/C29: real graph edges, distinct in meaning from a call.
        // Expressing them as calls would inflate the call graph with edges
        // that do not mean what the kind claims.
        let facts = ts("class D extends Base implements I, J {}\ninterface K extends L {}\n");
        let inherited: Vec<(&str, DeclSpace)> = refs(&facts, RefKind::Inherit)
            .iter()
            .map(|r| (r.raw_target.as_str(), r.space))
            .collect();
        assert_eq!(
            inherited,
            [
                ("Base", DeclSpace::Value),
                ("I", DeclSpace::Type),
                ("J", DeclSpace::Type),
                ("L", DeclSpace::Type),
            ]
        );
        // `extends mixin(Base)` is a call and nothing else; pretending it is
        // a heritage reference would invent a target.
        let mixin = js("class D extends mixin(Base) {}\n");
        assert!(refs(&mixin, RefKind::Inherit).is_empty());
        assert_eq!(refs(&mixin, RefKind::Call).len(), 1);
    }

    #[test]
    fn jsx_element_names_are_references() {
        // F11: skipping JSX loses most references in any React corpus. The
        // lowercase-is-intrinsic rule is a React transform convention rather
        // than part of the grammar, so every element is emitted and the
        // convention is the resolver's to apply.
        let facts = js("const t = <div><Button onClick={h} /><Icons.Star /></div>;\n");
        let calls = refs(&facts, RefKind::Call);
        let named: Vec<Vec<&str>> = calls.iter().map(|c| segments(c)).collect();
        assert!(named.contains(&vec!["div"]));
        assert!(named.contains(&vec!["Button"]));
        assert!(named.contains(&vec!["Icons", "Star"]));
        assert!(calls.iter().all(|c| c.argc.is_none()));
    }

    #[test]
    fn tagged_templates_and_optional_calls_need_no_extra_rule() {
        // F12: the grammar represents all three as `call_expression`.
        let facts = js("tag`x`;\nf?.();\nobj?.m();\n");
        let calls = refs(&facts, RefKind::Call);
        assert_eq!(calls.len(), 3);
        assert_eq!(segments(calls[0]), ["tag"]);
        assert_eq!(segments(calls[1]), ["f"]);
        assert_eq!(segments(calls[2]), ["obj", "m"]);
    }

    #[test]
    fn a_type_use_is_not_a_call_and_typeof_inverts_the_space() {
        // §6/C20/C21: `kind` is what the site does, `space` is which table
        // it reads, and `typeof x` is a type-position reference into the
        // **Value** space — which is why one axis cannot carry both.
        let facts = ts("type A = B;\nlet z: N.T;\ntype Q = typeof z;\n");
        let uses: Vec<(&str, DeclSpace)> = refs(&facts, RefKind::TypeUse)
            .iter()
            .map(|r| (r.raw_target.as_str(), r.space))
            .collect();
        assert!(uses.contains(&("B", DeclSpace::Type)));
        assert!(uses.contains(&("N.T", DeclSpace::Type)));
        assert!(uses.contains(&("z", DeclSpace::Value)));
        assert_eq!(
            segments(site(&facts, "N.T")),
            ["N", "T"],
            "a qualified type name is a path, not one opaque string"
        );
        // A declaration's own name is not a reference to itself.
        assert!(!uses.iter().any(|(raw, _)| *raw == "A" || *raw == "Q"));
    }

    #[test]
    fn a_decorator_is_one_annotation_and_not_also_a_call() {
        // C28. Emitting both would double-count every decorator in an
        // Angular or Nest corpus.
        let facts = ts("@Injectable()\nclass C { @Input() prop = 1; }\n");
        let annotations = refs(&facts, RefKind::Annotation);
        assert_eq!(annotations.len(), 2);
        assert_eq!(annotations[0].raw_target, "Injectable");
        assert_eq!(annotations[0].argc, Some(0));
        assert_eq!(annotations[1].raw_target, "Input");
        assert!(refs(&facts, RefKind::Call).is_empty());
    }

    #[test]
    fn argc_counts_arguments_and_distinguishes_zero_from_unknown() {
        let facts = js("g();\ng(1);\ng(1, 2);\ng(...xs);\n");
        let argc: Vec<Option<u32>> = refs(&facts, RefKind::Call).iter().map(|c| c.argc).collect();
        assert_eq!(argc, [Some(0), Some(1), Some(2), Some(1)]);
        // An import site has no argument list, and `None` says so rather
        // than standing in for zero.
        let import = js("import './x.js';\n");
        assert_eq!(refs(&import, RefKind::Import)[0].argc, None);
    }

    #[test]
    fn enclosing_is_the_nearest_nameable_definition() {
        // Anonymous functions are not nodes, so a call inside one belongs to
        // the named definition around it — the same rule Go applies to a
        // function literal.
        let facts = js(concat!(
            "function top(){ helper(); }\n",
            "const arrow = () => { helper(); };\n",
            "class C { m(){ items.forEach(() => helper()); } }\n",
            "const x = helper();\n",
            "helper();\n",
        ));
        let paths: Vec<Option<Vec<String>>> = refs(&facts, RefKind::Call)
            .iter()
            .filter(|r| r.raw_target == "helper")
            .map(|r| r.enclosing.as_ref().map(|e| e.path.clone()))
            .collect();
        assert_eq!(
            paths,
            [
                Some(vec!["top".into()]),
                Some(vec!["arrow".into()]),
                Some(vec!["C".into(), PROTOTYPE.into(), "m".into()]),
                Some(vec!["x".into()]),
                None,
            ]
        );
    }

    #[test]
    fn a_reference_carries_the_extractors_binding_verdict() {
        // D3/D4/D5, end to end: the extractor states the fact and the
        // resolver still owns the outcome. A false edge here is strictly
        // worse than an unresolved reference, because a miss is counted and
        // a wrong edge is not.
        let facts = js(concat!(
            "import { parse } from './p.js';\n",
            "parse();\n",
            "function f(){ const parse = 1; parse(); }\n",
            "function g(parse){ parse(); }\n",
            "items.forEach(save => save());\n",
        ));
        let verdicts: Vec<(&str, bool)> = refs(&facts, RefKind::Call)
            .iter()
            .map(|r| (r.raw_target.as_str(), r.locally_bound))
            .collect();
        assert_eq!(
            verdicts,
            [
                ("parse", false),
                ("parse", true),
                ("parse", true),
                ("items.forEach", false),
                ("save", true),
            ]
        );
    }

    #[test]
    fn only_the_root_of_a_chain_can_be_locally_bound() {
        // `x.y.z()` with `x` a parameter names a local however long the
        // member path is, which is why the target carries a root rather
        // than a `Local` variant.
        let facts = js("import cfg from './c.js';\nfunction f(cfg){ cfg.get().value(); }\n");
        assert!(site(&facts, "cfg.get").locally_bound);
        // The trailing call's operand is an expression, so no name of its
        // own can be bound.
        assert!(!site(&facts, "cfg.get().value").locally_bound);
    }

    #[test]
    fn a_type_parameter_binds_in_the_type_space_only() {
        // C23: the same class of bug as D5, one table over.
        let facts = ts("type T = string;\nfunction f<T>(x: T): T { return x; }\nlet z: T;\n");
        let uses: Vec<(&str, bool)> = refs(&facts, RefKind::TypeUse)
            .iter()
            .map(|r| (r.raw_target.as_str(), r.locally_bound))
            .collect();
        assert_eq!(uses, [("T", true), ("T", true), ("T", false)]);
    }
}
