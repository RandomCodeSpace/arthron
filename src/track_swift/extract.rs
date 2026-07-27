//! Swift extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/swift.yml`) select nodes by kind; this
//! module interprets their fields.
//!
//! # What a tier-2 extractor emits, and what it must not
//!
//! Definitions and structure, plus **import references and nothing else**.
//! Swift's gate is an import-resolution rate, so a call site, a type use or
//! an `inheritance_specifier` emitted here would enter a denominator nothing
//! in this track can resolve — tier-1 coverage claimed without tier-1
//! measurement. `class C: Base` is therefore read as part of `C`'s structure
//! and produces no [`RefKind::Inherit`] reference.
//!
//! # The one thing a Swift file never says
//!
//! **Which module it belongs to.** A Swift module is a whole SwiftPM
//! *target*, and every top-level name in it is visible to every other file of
//! the target with no import, no path and no qualifier anywhere in the
//! referencing file — measured on the corpus, not one of the 43 files in
//! Alamofire's `Source/` imports Alamofire. So the file's module placeholder
//! is emitted with an **empty name**: "this file does not say" is the
//! measured fact, and [`crate::track_swift::resolve`] supplies the identity
//! from the manifest. It is also why those 43 mutual visibilities produce no
//! reference at all — there is no site to emit one at, and inventing 43×42 of
//! them would be minting a denominator out of nothing.
//!
//! # An extension is not a declaration of the type it extends
//!
//! The corpus has 194 of them, and Swift lets a type's members be declared in
//! a file that does not declare the type — including on types the repository
//! does not own (`URLRequest`, `URLSession`, `Data`). So an `extension`
//! emits **no definition of its own**, and its members are filed under the
//! extended type's path: `extension URLRequest { func af() }` in module
//! `Alamofire` declares `Alamofire.URLRequest.af()`, which says the
//! repository declares that member without claiming it declares Foundation's
//! type. Emitting the extension as a `Type` would put `URLRequest` in the
//! repository's own definition table, which is false.
//!
//! The extensions are still counted, in [`SwiftHeader::extensions`], because
//! they are the one piece of Swift structure that produces no node — and a
//! census that cannot see 194 declarations is not a census.
//!
//! # A callable is named the way Swift names a declaration
//!
//! `request(_:method:)`, not `request`. Argument labels are part of a Swift
//! declaration's name — two overloads differing only in labels are two
//! declarations any Swift programmer spells differently — so folding them
//! into one node would be a census that under-counts the API surface it
//! claims to measure. Overloads differing only in *types* still share a name;
//! that shortfall is recorded below rather than hidden.
//!
//! # Recorded under-counts
//!
//! Each is a known shortfall, written down rather than left to be
//! rediscovered, and none may be closed by widening a bucket:
//!
//! - **Overloads that differ only in parameter types** — `f(_ x: Int)` and
//!   `f(_ x: String)` — share the name `f(_:)` and so share a node. Telling
//!   them apart needs the types in the identity, which is a decision for the
//!   tier that resolves type uses.
//! - **A declaration inside a body is not emitted.** A `let` in a function
//!   body is a local and a `func` in one is a closure by another spelling;
//!   neither is a name another file can write. The same rule takes out
//!   declarations inside a computed property's accessors.
//! - **A tuple binding contributes each name it binds**, but a `_` binds
//!   nothing and produces nothing.
//! - **`operator` and `precedencegroup` declarations are not emitted.** The
//!   measured corpus contains none of either, and neither is a name a member
//!   lookup reaches. A `func ~>` *is* emitted, under the operator as written.
//! - **Macro declarations and macro invocations are not read.** The corpus
//!   contains 183 invocations and no declaration; a macro expands to
//!   declarations this build does not run, which is what
//!   [`crate::UnresolvedReason::Generated`] exists to say at a tier that
//!   resolves them.
//! - **Generic parameters are not definitions.** Nothing at tier 2 names one.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, RefKind, RefTarget, Reference, Span, TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_swift::lang::SwiftLang;

/// The embedded Swift extraction rules.
const SWIFT_RULES: &str = include_str!("../rules/swift.yml");

/// One `import` clause: what it names, how it is written, and where it sits.
///
/// Every `ImportSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is how a
/// corpus check can prove no clause was dropped on the way to a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    /// The dotted path as written, module first: `["Foundation"]`,
    /// `["Foundation", "Data"]` for `import struct Foundation.Data`.
    pub path: Vec<String>,
    /// The clause carries `@testable`.
    ///
    /// Recorded, not acted on. `@testable` widens the imported module's
    /// `internal` declarations into scope, which changes which *members* a
    /// name can reach and changes nothing about *which module* is named — and
    /// naming the module is the whole of what this tier resolves. A facet of
    /// the measurement, in other words, and never a resolution rule or a
    /// reason of its own.
    pub testable: bool,
    /// Where the clause sits: the whole declaration, attributes included.
    pub span: Span,
}

/// One `extension` declaration: the type it extends, and where it sits.
///
/// An extension declares members without declaring a node of its own, so
/// nothing in [`FileFacts::defs`] can be counted to find one. This is the
/// only record of them, and it exists so a census can state how much of the
/// repository's structure arrives through extensions rather than leaving that
/// number unmeasurable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSite {
    /// The extended type's path as written, outermost first: `["URLRequest"]`,
    /// `["Outer", "Inner"]`.
    pub extended: Vec<String>,
    /// Where the declaration sits.
    pub span: Span,
}

/// Per-file Swift facts.
///
/// `rel_path` is what the resolver reads: a Swift file's module is decided by
/// which target's directory it sits in, and the core must not be the layer
/// that turns a path into a module. The other two fields are measured
/// structure the record set cannot otherwise show — see [`ImportSpec`] and
/// [`ExtensionSite`] for why each is here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SwiftHeader {
    /// Repo-relative, `/`-separated path of the file.
    pub rel_path: String,
    /// Every import clause, in source order.
    pub imports: Vec<ImportSpec>,
    /// Every extension declaration, in source order.
    pub extensions: Vec<ExtensionSite>,
}

/// Node kinds a nameable declaration may be nested inside.
///
/// A whitelist and not a blacklist: everything else between a declaration and
/// the top of its file — a function body, a closure, an accessor, a switch
/// arm — makes the declaration a local, and a local is not a node by
/// decision. Answering "no" for an unknown kind is what keeps a node from
/// being invented for a declaration whose owner this file does not state.
///
/// `ERROR` is on the list because tree-sitter emits an empty error node where
/// a `#if` opens inside a type body. Both arms stay siblings in the body, so
/// crossing it changes no owner — and a genuinely broken region cannot smuggle
/// anything through, because a declaration is still matched by its own kind.
const DECLARATION_FRAMES: &[&str] = &[
    "class_body",
    "enum_class_body",
    "protocol_body",
    "ERROR",
    "source_file",
];

/// The owner chain a declaration sits in, outermost first.
///
/// `None` when the declaration is not nameable — see [`DECLARATION_FRAMES`].
fn owner_of(node: &SgNode) -> Option<Vec<String>> {
    let mut chain: Vec<String> = Vec::new();
    for a in node.ancestors() {
        let kind = a.kind().to_string();
        if kind == "source_file" {
            break;
        }
        if DECLARATION_FRAMES.contains(&kind.as_str()) {
            continue;
        }
        if kind != "class_declaration" && kind != "protocol_declaration" {
            return None;
        }
        let name = a.field("name")?.text().to_string();
        if is_extension(&a) {
            // An extension is a top-level declaration, so it is the outermost
            // frame there is; its members belong to the type it extends,
            // spelled the way the extension head spells it.
            for segment in name.split('.').rev() {
                chain.push(segment.to_string());
            }
        } else {
            chain.push(name);
        }
    }
    chain.reverse();
    Some(chain)
}

/// Whether a `class_declaration` is an `extension`.
fn is_extension(node: &SgNode) -> bool {
    node.field("declaration_kind")
        .is_some_and(|k| k.text() == "extension")
}

/// The `modifiers` a declaration carries, as one text per modifier node.
fn modifier_texts(node: &SgNode) -> Vec<String> {
    let Some(list) = node.children().find(|c| c.kind() == "modifiers") else {
        return Vec::new();
    };
    list.children().map(|m| m.text().to_string()).collect()
}

/// The facets every Swift declaration shares: visibility and staticness.
fn common_facets(node: &SgNode) -> DefFacets {
    let mut facets = DefFacets::default();
    for m in modifier_texts(node) {
        match m.as_str() {
            "public" | "open" | "package" => facets = facets.union(DefFacets::EXPORTED),
            // `private(set)` narrows the setter and leaves the declaration
            // itself as visible as it was; only the bare spellings take a
            // member out of what another file can name.
            "private" | "fileprivate" => facets = facets.union(DefFacets::PRIVATE),
            "static" => facets = facets.union(DefFacets::STATIC),
            _ => {}
        }
    }
    // `class func` is `static` with an override; the keyword sits outside the
    // modifier list.
    if node.children().any(|c| c.kind() == "class") {
        facets = facets.union(DefFacets::STATIC);
    }
    facets
}

/// One definition, with the fields every Swift declaration shares.
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

/// A callable's declaration name: the base name plus its argument labels.
///
/// `f()`, `g(_:)`, `request(_:method:)` — Swift's own spelling. A parameter
/// writes its external label first when it writes two names, and a parameter
/// that writes one uses it as both.
fn callable_name(node: &SgNode, base: &str) -> String {
    let mut out = String::from(base);
    out.push('(');
    for p in node.children().filter(|c| c.kind() == "parameter") {
        match p.children().find(|c| c.kind() == "simple_identifier") {
            Some(label) => out.push_str(&label.text()),
            // A parameter with no identifier at all is not a shape the
            // grammar produces for valid Swift; `_` is the honest stand-in
            // and never silently drops the position.
            None => out.push('_'),
        }
        out.push(':');
    }
    out.push(')');
    out
}

/// Every name a `pattern` binds, in source order. A `_` binds none.
fn pattern_names(pattern: &SgNode, out: &mut Vec<String>) {
    if let Some(bound) = pattern.field("bound_identifier") {
        out.push(bound.text().to_string());
        return;
    }
    for child in pattern.children().filter(|c| c.kind() == "pattern") {
        pattern_names(&child, out);
    }
}

/// A string literal's value, or `None` when it is not one plain literal.
///
/// `pub(crate)` because phase 0 reads the same shape out of a package
/// manifest — one reader for "is this argument a literal string?", not two
/// that can drift. Interpolation and escapes answer `None`: a value this
/// function cannot read is one no layer above may approximate.
pub(crate) fn string_literal(node: &SgNode) -> Option<String> {
    if node.kind() != "line_string_literal" {
        return None;
    }
    let mut out = String::new();
    for child in node.children() {
        match &*child.kind() {
            "line_str_text" => out.push_str(&child.text()),
            "\"" => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Extract one Swift file. The whole of the extractor's public surface.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<SwiftLang> {
    static RULES: OnceLock<Rules> = OnceLock::new();
    let rules = RULES.get_or_init(|| Rules::compile(SWIFT_RULES).expect("swift.yml compiles"));

    let mut facts: FileFacts<SwiftLang> = FileFacts {
        header: SwiftHeader {
            rel_path: rel_path.to_string(),
            imports: Vec::new(),
            extensions: Vec::new(),
        },
        defs: Vec::new(),
        refs: Vec::new(),
    };

    // The file's module placeholder, first, because the driver reads the
    // first `Module` definition as the file's container. Its name is empty on
    // purpose: no Swift file states which module it belongs to, and the
    // resolver supplies the identity from the manifest.
    facts.defs.push(def(
        DefKind::Module,
        String::new(),
        Vec::new(),
        DeclSpace::Namespace,
        DefFacets::SYNTHETIC,
        Span {
            byte_start: 0,
            byte_end: source.len() as u32,
            line: 1,
        },
    ));

    let tree = SourceTree::parse_swift(source);
    for (rule, node) in tree.matches(rules) {
        match rule {
            "import" => import(&mut facts, &node),
            "def-type" => type_declaration(&mut facts, &node),
            "def-function" => function(&mut facts, &node),
            "def-init" => callable(&mut facts, &node, DefKind::Constructor, "init"),
            "def-deinit" => deinit(&mut facts, &node),
            "def-subscript" => callable(&mut facts, &node, DefKind::Property, "subscript"),
            "def-property" => property(&mut facts, &node),
            "def-typealias" => simple_type(&mut facts, &node, DefKind::Alias, DefFacets::default()),
            "def-associatedtype" => {
                simple_type(&mut facts, &node, DefKind::Type, DefFacets::ABSTRACT)
            }
            "def-enum-case" => enum_case(&mut facts, &node),
            _ => {}
        }
    }
    // Rules run one at a time, so the records arrive rule-major; source order
    // is what a reader of a report expects and what a span-keyed pairing
    // needs to be stable under. The sort is stable, so two names bound by one
    // declaration keep the order they were written in.
    facts.defs[1..].sort_by_key(|d| d.span.byte_start);
    facts.refs.sort_by_key(|r| r.span.byte_start);
    facts.header.imports.sort_by_key(|i| i.span.byte_start);
    facts.header.extensions.sort_by_key(|e| e.span.byte_start);
    facts
}

/// `import Foundation`, `@testable import Alamofire`,
/// `import struct Foundation.Data`.
fn import(facts: &mut FileFacts<SwiftLang>, node: &SgNode) {
    let Some(path_node) = node.children().find(|c| c.kind() == "identifier") else {
        return; // `import` with nothing after it is not an import site
    };
    let path: Vec<String> = path_node
        .children()
        .filter(|c| c.kind() == "simple_identifier")
        .map(|c| c.text().to_string())
        .collect();
    if path.is_empty() {
        return;
    }
    let testable = node
        .children()
        .find(|c| c.kind() == "modifiers")
        .is_some_and(|m| {
            m.children()
                .any(|a| a.kind() == "attribute" && a.text().trim() == "@testable")
        });
    let span = span_of(node);
    // The declaration as written, with the line breaks a split attribute
    // introduces folded out. This is what a `RefKey` is keyed on and what a
    // query prints back, so it must tell two clauses in one file apart:
    // `import Alamofire` and `@_spi(WebSocket) import Alamofire` are two
    // sites naming one module, and the corpus contains exactly that pair.
    let raw_target = node.text().split_whitespace().collect::<Vec<_>>().join(" ");
    facts.header.imports.push(ImportSpec {
        path: path.clone(),
        testable,
        span,
    });
    facts.refs.push(Reference {
        kind: RefKind::Import,
        space: DeclSpace::Namespace,
        raw_target,
        target: RefTarget {
            root: TargetRoot::Name,
            segments: path,
        },
        // Tier 2 emits no expression-level reference, so nothing here can
        // name a local: `LocalBinding` does not apply to this track.
        locally_bound: false,
        argc: None,
        // A Swift `import` is a file-scope declaration, so there is never a
        // nameable definition between it and the file's module; the driver
        // sources every one of them at the module itself.
        enclosing: None,
        span,
    });
}

/// `class`, `struct`, `enum`, `actor`, `protocol` — and `extension`, which
/// declares no type.
fn type_declaration(facts: &mut FileFacts<SwiftLang>, node: &SgNode) {
    let Some(owner) = owner_of(node) else { return };
    let Some(name) = node.field("name").map(|n| n.text().to_string()) else {
        return;
    };
    if is_extension(node) {
        facts.header.extensions.push(ExtensionSite {
            extended: name.split('.').map(str::to_string).collect(),
            span: span_of(node),
        });
        return;
    }
    let kind = node
        .field("declaration_kind")
        .map(|k| k.text().to_string())
        .unwrap_or_default();
    let mut facets = common_facets(node);
    match kind.as_str() {
        "enum" => facets = facets.union(DefFacets::ENUM),
        "protocol" => facets = facets.union(DefFacets::INTERFACE),
        _ => {}
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

/// `func f()` at any level, and a protocol's `func` requirement.
fn function(facts: &mut FileFacts<SwiftLang>, node: &SgNode) {
    let Some(owner) = owner_of(node) else { return };
    let Some(base) = node.field("name").map(|n| n.text().to_string()) else {
        return;
    };
    let kind = if owner.is_empty() {
        DefKind::Function
    } else {
        DefKind::Method
    };
    let mut facets = common_facets(node);
    if node.kind() == "protocol_function_declaration" {
        facets = facets.union(DefFacets::ABSTRACT);
    }
    facts.defs.push(def(
        kind,
        callable_name(node, &base),
        owner,
        DeclSpace::Value,
        facets,
        span_of(node),
    ));
}

/// `init` and `subscript`: a declaration whose base name is the keyword.
fn callable(facts: &mut FileFacts<SwiftLang>, node: &SgNode, kind: DefKind, base: &str) {
    let Some(owner) = owner_of(node) else { return };
    let mut facets = common_facets(node);
    // A protocol's `init()` requirement is an `init_declaration` with no
    // body, like every other requirement beside it.
    if node.parent().is_some_and(|p| p.kind() == "protocol_body") {
        facets = facets.union(DefFacets::ABSTRACT);
    }
    facts.defs.push(def(
        kind,
        callable_name(node, base),
        owner,
        DeclSpace::Value,
        facets,
        span_of(node),
    ));
}

/// `deinit`: one per type, and the only declaration Swift gives no name.
fn deinit(facts: &mut FileFacts<SwiftLang>, node: &SgNode) {
    let Some(owner) = owner_of(node) else { return };
    if owner.is_empty() {
        return; // a `deinit` outside a type declares nothing
    }
    facts.defs.push(def(
        DefKind::Method,
        "deinit".to_string(),
        owner,
        DeclSpace::Value,
        common_facets(node),
        span_of(node),
    ));
}

/// `let`/`var` at file or type scope, and a protocol's `var` requirement.
fn property(facts: &mut FileFacts<SwiftLang>, node: &SgNode) {
    let Some(owner) = owner_of(node) else { return };
    let mut names = Vec::new();
    for pattern in node.children().filter(|c| c.kind() == "pattern") {
        pattern_names(&pattern, &mut names);
    }
    if names.is_empty() {
        return; // `let _ = …` binds nothing a reference could name
    }
    let is_protocol_requirement = node.kind() == "protocol_property_declaration";
    // A computed property has accessors and no storage; an observer is not an
    // accessor, so `var x = 0 { didSet { … } }` is stored and stays a field.
    let computed =
        is_protocol_requirement || node.children().any(|c| c.kind() == "computed_property");
    let mutable = node
        .children()
        .any(|c| c.kind() == "value_binding_pattern" && c.text().trim() == "var")
        || node.children().any(|c| {
            c.kind() == "pattern"
                && c.children()
                    .any(|g| g.kind() == "value_binding_pattern" && g.text().trim() == "var")
        });
    let kind = match (owner.is_empty(), computed, mutable) {
        // At file scope a binding is a constant or a variable of the module.
        (true, _, false) => DefKind::Const,
        (true, _, true) => DefKind::Var,
        (false, true, _) => DefKind::Property,
        (false, false, _) => DefKind::Field,
    };
    let mut facets = common_facets(node);
    if is_protocol_requirement {
        facets = facets.union(DefFacets::ABSTRACT);
    }
    let span = span_of(node);
    for name in names {
        facts.defs.push(def(
            kind,
            name,
            owner.clone(),
            DeclSpace::Value,
            facets,
            span,
        ));
    }
}

/// `typealias T = …` and `associatedtype T`.
fn simple_type(facts: &mut FileFacts<SwiftLang>, node: &SgNode, kind: DefKind, extra: DefFacets) {
    let Some(owner) = owner_of(node) else { return };
    let Some(name) = node.field("name").map(|n| n.text().to_string()) else {
        return;
    };
    facts.defs.push(def(
        kind,
        name,
        owner,
        DeclSpace::Type,
        common_facets(node).union(extra),
        span_of(node),
    ));
}

/// `case a`, `case b, c`, `case d(Int)` — one constant per name written.
fn enum_case(facts: &mut FileFacts<SwiftLang>, node: &SgNode) {
    let Some(owner) = owner_of(node) else { return };
    if owner.is_empty() {
        return; // a `case` outside an enum body declares nothing
    }
    let span = span_of(node);
    for name in node.children().filter(|c| c.kind() == "simple_identifier") {
        facts.defs.push(def(
            DefKind::Const,
            name.text().to_string(),
            owner.clone(),
            DeclSpace::Value,
            DefFacets::default(),
            span,
        ));
    }
}

/// The Swift extractor, as the driver holds it.
pub struct SwiftExtractor;

impl Extractor<SwiftLang> for SwiftExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<SwiftLang> {
        extract(rel_path, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_compile() {
        Rules::compile(SWIFT_RULES).expect("swift.yml compiles");
    }

    #[test]
    fn a_string_literal_that_is_not_one_plain_literal_reads_as_none() {
        // Phase 0 depends on this: a target path built by interpolation is
        // one no manifest reader may approximate.
        let tree = SourceTree::parse_swift("let a = \"Source\"\nlet b = \"S\\(x)\"\n");
        let rules = Rules::compile("id: s\nlanguage: swift\nrule:\n  kind: line_string_literal\n")
            .expect("rules");
        let found = tree.matches(&rules);
        assert_eq!(found.len(), 2);
        assert_eq!(string_literal(&found[0].1), Some("Source".to_string()));
        assert_eq!(string_literal(&found[1].1), None);
    }
}
