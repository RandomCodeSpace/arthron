//! One Kotlin file in, records out. Forbidden from linking.
//!
//! # What a tier-2 extractor emits
//!
//! **Definitions and structure, and imports.** Nothing else. The reference
//! kind this module produces is [`RefKind::Import`] and only that: no call
//! site, no type use, no supertype. `class Buffer : Sink` is read as part of
//! `Buffer`'s structure and produces no [`RefKind::Inherit`]; `@Throws(IOException::class)`
//! produces no [`RefKind::Annotation`]. A tier-2 language that emitted them
//! would put references into a denominator no tier-2 resolver links, which is
//! tier-1 coverage claimed without tier-1 work.
//!
//! # A declaration is a node only when every frame above it can be named
//!
//! [`owner_chain`] walks a declaration's ancestors and answers `None` unless
//! **every** one of them is on a short allow-list: a class body, an enum
//! class body, a primary constructor, a classifier declaration, or the file
//! itself. Anything else — a function body, a getter, an `init` block, an
//! `object : Runnable { … }` literal, an enum entry's own body, a lambda, an
//! `ERROR` node — means the declaration has no owner this file states, and no
//! node is invented for it.
//!
//! An allow-list rather than a deny-list, because the pinned grammar does not
//! always announce that it lost its place. okio writes
//!
//! ```text
//! expect open class ByteString
//! // Trusted internal constructor doesn't clone data.
//! internal constructor(data: ByteArray) : Comparable<ByteString> {
//! ```
//!
//! and tree-sitter-kotlin 0.4.1 cannot parse a comment or a modified primary
//! constructor written on the line after a class header. Six corpus files hit
//! it. In all six the class *body* comes back inside a `lambda_literal`
//! hanging off an expression beside the declaration rather than inside it; in
//! two of them the declaration goes with it, and one of those two —
//! `appleMain/okio/ByteString.kt` — parses into a `comparison_expression`
//! with **no error node at all**. A deny-list would have emitted every one of
//! those members as a *top-level* declaration of package `okio`, minting
//! `okio#utf8()` where the source wrote `ByteString.utf8()`. Wrong
//! definitions are worse than missing ones, so the misparsed files lose their
//! members and say so — see [`crate::track_kotlin`] for the count.
//!
//! # Known under-counts, recorded rather than left to be rediscovered
//!
//! - **A declaration inside a function body, a getter, a setter or an `init`
//!   block.** A local class or a local function is real, and Kotlin gives it
//!   no name anything outside the body can spell. Not a node, by the same
//!   judgement Ruby makes for a declaration inside a block.
//! - **A member declared in an enum entry's body.** `Empty { override fun
//!   newBuffer() … }` declares on an anonymous subclass of the enum, which
//!   has no canonical name — the judgement Java's track makes for an
//!   enum-constant body.
//! - **A property's `get()`/`set()` accessors.** They are part of the
//!   property, which is already a node; a separate node per accessor would be
//!   two nodes for one slot.
//! - **A destructured `val (a, b)`.** Each name is emitted, because each is a
//!   declaration; Kotlin permits the form only locally, so in practice the
//!   ancestor allow-list drops them first.
//! - **`@file:JvmName("-Okio")` and the other 30 `@file:Jvm*` annotations.**
//!   They rename what a *Java* caller sees. Nothing in the Kotlin reference
//!   space names the renamed form, and inventing a second identity for it
//!   would mint a node no Kotlin import can reach.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_kotlin::lang::{COMPANION, INIT, KtLang, ON_DEMAND};

/// The embedded Kotlin extraction rules.
const KOTLIN_RULES: &str = include_str!("../rules/kotlin.yml");

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules::compile(KOTLIN_RULES).expect("embedded kotlin.yml compiles"))
}

/// One import clause: what it names, how, and where it sits.
///
/// Every `ImportSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is what
/// makes "an import clause that produced no reference" a checkable statement
/// rather than a silent drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    /// `import okio.*` — every member of what the path names.
    pub on_demand: bool,
    /// The name the clause binds in this file, when it renames.
    ///
    /// Carried because it is what the site says, and read by nothing here:
    /// an alias binds a name for expression-level references, and tier 2
    /// emits none. It is part of the reference's `raw_target` so that two
    /// imports of one target under two aliases stay two rows.
    pub alias: Option<String>,
    /// Where the clause sits — the whole `import` header, so the key is
    /// unique within the file.
    pub span: Span,
}

/// What one Kotlin file states about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KtHeader {
    /// The file's repository-relative path.
    pub rel_path: String,
    /// The package the file declares. `""` is the default package — a
    /// container with no name, which is a different fact from a file naming
    /// no container. Kotlin permits at most one `package` header per file, so
    /// unlike PHP's namespaces this really is a function of the file.
    pub package: String,
    /// Every import clause, in source order.
    pub imports: Vec<ImportSpec>,
}

/// The Kotlin extractor. One path and one source string; nothing to link with.
pub struct KtExtractor;

impl Extractor<KtLang> for KtExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<KtLang> {
        extract(rel_path, source)
    }
}

/// Ancestor kinds a declaration may sit under and still be nameable.
///
/// `class_body` and `enum_class_body` are the bodies a classifier owns;
/// `primary_constructor` is the parameter list a `val`/`var` parameter
/// declares a property in; `source_file` is the top. The three classifier
/// kinds contribute a name and are handled separately.
const TRANSPARENT: [&str; 4] = [
    "source_file",
    "class_body",
    "enum_class_body",
    "primary_constructor",
];

/// Classifier declarations, which are the only ancestors that name a frame.
const CLASSIFIER: [&str; 3] = [
    "class_declaration",
    "object_declaration",
    "companion_object",
];

/// The classifier chain enclosing a node, outermost first.
///
/// `None` when any frame between the node and the file is one no lexical
/// name reaches. See the module header for why this is an allow-list.
fn owner_chain(node: &SgNode) -> Option<Vec<String>> {
    let mut chain: Vec<String> = Vec::new();
    for a in node.ancestors() {
        let kind = a.kind();
        if TRANSPARENT.contains(&&*kind) {
            continue;
        }
        if CLASSIFIER.contains(&&*kind) {
            chain.push(declared_name(&a)?);
            continue;
        }
        return None;
    }
    chain.reverse();
    Some(chain)
}

/// The name a classifier declaration writes.
///
/// An unnamed `companion object` is declared under the name Kotlin gives it,
/// which is also the one an import spells.
fn declared_name(node: &SgNode) -> Option<String> {
    let written = node
        .children()
        .find(|c| matches!(&*c.kind(), "type_identifier" | "simple_identifier"))
        .map(|c| c.text().to_string());
    match (node.kind() == "companion_object", written) {
        (_, Some(name)) => Some(name),
        (true, None) => Some(COMPANION.to_string()),
        (false, None) => None,
    }
}

/// The identifiers a dotted `identifier` node joins, in source order.
///
/// Read off the children rather than the node's text: the text of an
/// `import` path may carry a line comment between two segments, and a
/// segment is an identifier or the path is not one.
fn dotted(node: &SgNode) -> Vec<String> {
    node.children()
        .filter(|c| c.kind() == "simple_identifier")
        .map(|c| c.text().to_string())
        .collect()
}

/// Whether anything under this node is an error node.
///
/// Recursive, because the grammar's recovery point is not fixed.
fn has_error(node: &SgNode) -> bool {
    node.kind() == "ERROR" || node.children().any(|child| has_error(&child))
}

/// The text of a modifier of one kind, when the declaration carries one.
fn modifier(node: &SgNode, kind: &str) -> Option<String> {
    node.children()
        .find(|c| c.kind() == "modifiers")?
        .children()
        .find(|c| c.kind() == kind)
        .map(|c| c.text().trim().to_string())
}

/// Whether a declaration carries a direct child token of this kind.
///
/// `enum class` and `interface` are spelled as keywords rather than as
/// modifiers, so this is how a `class_declaration` says which it is.
fn has_token(node: &SgNode, kind: &str) -> bool {
    node.children().any(|c| c.kind() == kind)
}

/// The facets every declaration shares: what its visibility says.
///
/// Kotlin's default is public, so a declaration with no visibility modifier
/// is exported. `internal` is module-wide rather than declaration-local, so
/// it is *not* [`DefFacets::PRIVATE`] — that bit means "not inherited by
/// anything below it", which `internal` does not say.
fn visibility_facets(node: &SgNode) -> DefFacets {
    match modifier(node, "visibility_modifier").as_deref() {
        None | Some("public") => DefFacets::EXPORTED,
        Some("private") => DefFacets::PRIVATE,
        _ => DefFacets::default(),
    }
}

/// One definition, with the fields every Kotlin declaration shares.
fn kt_def(
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
        // A callable key is a name and not a name plus an arity — see
        // `lang.rs` for why tier 2 stops there — so nothing here
        // discriminates by parameter shape.
        params: None,
        span,
    }
}

/// Extract all facts from one Kotlin source file.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<KtLang> {
    let tree = SourceTree::parse_kotlin(source);
    let matches = tree.matches(rules());

    // Kotlin permits one `package` header per file; a file with none is in
    // the default package, whose name is the empty string.
    let package_header = matches
        .iter()
        .map(|(_, n)| n)
        .find(|n| n.kind() == "package_header");
    let package = package_header.map_or_else(String::new, |n| {
        n.children()
            .find(|c| c.kind() == "identifier")
            .map(|c| dotted(&c).join("."))
            .unwrap_or_default()
    });

    let mut facts: FileFacts<KtLang> = FileFacts {
        header: KtHeader {
            rel_path: rel_path.to_string(),
            package: package.clone(),
            imports: Vec::new(),
        },
        defs: Vec::new(),
        refs: Vec::new(),
    };

    // The file's container, first, because the driver reads the first
    // `Module` definition as the file's package. Emitted whether or not the
    // file writes a header: every declaration needs a container, and every
    // import needs an edge source.
    facts.defs.push(kt_def(
        DefKind::Module,
        package.clone(),
        Vec::new(),
        DeclSpace::Namespace,
        DefFacets::default(),
        package_header.map_or(
            Span {
                byte_start: 0,
                byte_end: source.len().min(u32::MAX as usize) as u32,
                line: 1,
            },
            span_of,
        ),
    ));

    for (_, node) in &matches {
        match &*node.kind() {
            "package_header" => {}
            "import_header" => import(&mut facts, node, &package),
            "class_declaration" | "object_declaration" | "companion_object" => {
                classifier(&mut facts, node, &package);
            }
            "type_alias" => alias(&mut facts, node, &package),
            "function_declaration" => function(&mut facts, node, &package),
            "property_declaration" => property(&mut facts, node, &package),
            "primary_constructor" | "secondary_constructor" => {
                constructor(&mut facts, node, &package);
            }
            "class_parameter" => class_parameter(&mut facts, node, &package),
            "enum_entry" => enum_entry(&mut facts, node, &package),
            _ => {}
        }
    }

    // One rule means document order already, but a span-keyed pairing and a
    // report both want it stated rather than inherited. `defs[0]` is the
    // container and stays first.
    facts.defs[1..].sort_by_key(|d| d.span.byte_start);
    facts.refs.sort_by_key(|r| r.span.byte_start);
    facts.header.imports.sort_by_key(|i| i.span.byte_start);
    facts
}

/// The owner a declaration under `node` carries: the package, then the
/// classifier chain. `None` when a frame above it cannot be named.
fn owner_of(node: &SgNode, package: &str) -> Option<Vec<String>> {
    let mut owner = vec![package.to_string()];
    owner.extend(owner_chain(node)?);
    Some(owner)
}

/// One `import` header.
fn import(facts: &mut FileFacts<KtLang>, node: &SgNode, package: &str) {
    // A header the grammar did not understand states nothing. Reading a
    // partial path would mint an import the source never wrote, and a short
    // path is exactly the shape that resolves to the wrong package.
    if has_error(node) {
        return;
    }
    let Some(path) = node.children().find(|c| c.kind() == "identifier") else {
        return;
    };
    let segments = dotted(&path);
    if segments.is_empty() {
        return;
    }
    let on_demand = node.children().any(|c| c.kind() == "wildcard_import");
    let alias = node
        .children()
        .find(|c| c.kind() == "import_alias")
        .and_then(|c| {
            c.children()
                .find(|n| matches!(&*n.kind(), "type_identifier" | "simple_identifier"))
                .map(|n| n.text().to_string())
        });

    let mut raw_target = segments.join(".");
    if on_demand {
        raw_target.push('.');
        raw_target.push_str(ON_DEMAND);
    }
    if let Some(name) = &alias {
        raw_target.push_str(" as ");
        raw_target.push_str(name);
    }
    let mut target = segments;
    if on_demand {
        target.push(ON_DEMAND.to_string());
    }

    let span = span_of(node);
    facts.header.imports.push(ImportSpec {
        on_demand,
        alias,
        span,
    });
    facts.refs.push(Reference {
        kind: RefKind::Import,
        // A Kotlin import names a member of a package, so the table it reads
        // is the package's own; whether what it finds is a classifier or a
        // callable is a property of the answer, not of the question.
        space: DeclSpace::Namespace,
        raw_target,
        target: RefTarget {
            root: TargetRoot::Name,
            segments: target,
        },
        // Structurally false: an import names a package-qualified path, and
        // no block binds one. `LocalBinding` does not apply at tier 2 —
        // there is no expression-level reference to be bound.
        locally_bound: false,
        argc: None,
        enclosing: Some(Encloser {
            path: vec![package.to_string()],
            kind: DefKind::Module,
        }),
        span,
    });
}

/// `class`, `interface`, `enum class`, `annotation class`, `object` and
/// `companion object`.
fn classifier(facts: &mut FileFacts<KtLang>, node: &SgNode, package: &str) {
    let Some(owner) = owner_of(node, package) else {
        return;
    };
    let Some(name) = declared_name(node) else {
        return;
    };
    let mut facets = visibility_facets(node);
    if has_token(node, "interface") {
        facets = facets.union(DefFacets::INTERFACE);
    }
    if has_token(node, "enum") {
        facets = facets.union(DefFacets::ENUM);
    }
    if modifier(node, "class_modifier").as_deref() == Some("annotation") {
        facets = facets.union(DefFacets::ANNOTATION);
    }
    if modifier(node, "inheritance_modifier").as_deref() == Some("abstract") {
        facets = facets.union(DefFacets::ABSTRACT);
    }
    // An `object` and a `companion object` are each one instance, reached
    // through the name rather than through a receiver.
    if matches!(&*node.kind(), "object_declaration" | "companion_object") {
        facets = facets.union(DefFacets::STATIC);
    }
    facts.defs.push(kt_def(
        DefKind::Type,
        name,
        owner,
        DeclSpace::Type,
        facets,
        span_of(node),
    ));
}

/// `typealias Lock = ReentrantLock`.
///
/// An alias is a classifier: it takes the classifier keyspace, because
/// `actual typealias IOException = java.io.IOException` is the *same* name
/// `expect class IOException` declares one source set over. What it forwards
/// to is not recorded — the target is a type use, and tier 2 resolves none.
fn alias(facts: &mut FileFacts<KtLang>, node: &SgNode, package: &str) {
    let Some(owner) = owner_of(node, package) else {
        return;
    };
    let Some(name) = node
        .children()
        .find(|c| c.kind() == "type_identifier")
        .map(|c| c.text().to_string())
    else {
        return;
    };
    facts.defs.push(kt_def(
        DefKind::Alias,
        name,
        owner,
        DeclSpace::Type,
        visibility_facets(node),
        span_of(node),
    ));
}

/// `fun`, at the top level of a file or inside a classifier.
fn function(facts: &mut FileFacts<KtLang>, node: &SgNode, package: &str) {
    let Some(owner) = owner_of(node, package) else {
        return;
    };
    let Some(name) = node
        .children()
        .find(|c| c.kind() == "simple_identifier")
        .map(|c| c.text().to_string())
    else {
        return;
    };
    let mut facets = visibility_facets(node);
    if modifier(node, "inheritance_modifier").as_deref() == Some("abstract") {
        facets = facets.union(DefFacets::ABSTRACT);
    }
    // An extension function's receiver is not part of the name an import
    // spells — `import okio.internal.commonWrite` names the function whatever
    // it extends — so it is not part of the identity either.
    let kind = if owner.len() == 1 {
        DefKind::Function
    } else {
        DefKind::Method
    };
    facts.defs.push(kt_def(
        kind,
        name,
        owner,
        DeclSpace::Value,
        facets,
        span_of(node),
    ));
}

/// `val` and `var`, at the top level of a file or inside a classifier.
///
/// One definition per declared name: a destructuring `val (a, b)` declares
/// two, the way PHP's `property_declaration` declares one per element.
fn property(facts: &mut FileFacts<KtLang>, node: &SgNode, package: &str) {
    let Some(owner) = owner_of(node, package) else {
        return;
    };
    let mut facets = visibility_facets(node);
    if modifier(node, "inheritance_modifier").as_deref() == Some("abstract") {
        facets = facets.union(DefFacets::ABSTRACT);
    }
    // `const val` is a compile-time constant; every other `val` is a
    // read-only property, which is an accessor pair however it is written.
    let kind = match modifier(node, "property_modifier").as_deref() {
        Some("const") => DefKind::Const,
        _ => DefKind::Property,
    };
    for decl in node
        .children()
        .filter(|c| c.kind() == "variable_declaration")
    {
        let Some(name) = decl
            .children()
            .find(|c| c.kind() == "simple_identifier")
            .map(|c| c.text().to_string())
        else {
            continue;
        };
        facts.defs.push(kt_def(
            kind,
            name,
            owner.clone(),
            DeclSpace::Value,
            facets,
            span_of(&decl),
        ));
    }
}

/// A primary or secondary constructor.
///
/// Emitted only where the source writes one: `class Path(bytes: ByteString)`
/// has a `primary_constructor` node and `class Nested` has none, so the
/// census counts constructors a reader can point at rather than the implicit
/// one every class has.
fn constructor(facts: &mut FileFacts<KtLang>, node: &SgNode, package: &str) {
    let Some(owner) = owner_of(node, package) else {
        return;
    };
    if owner.len() == 1 {
        return; // a constructor with no classifier above it is a recovery artefact
    }
    facts.defs.push(kt_def(
        DefKind::Constructor,
        INIT.to_string(),
        owner,
        DeclSpace::Value,
        visibility_facets(node),
        span_of(node),
    ));
}

/// A primary-constructor parameter written `val` or `var` declares a
/// property of the class — Kotlin's own form of PHP's promoted property.
fn class_parameter(facts: &mut FileFacts<KtLang>, node: &SgNode, package: &str) {
    if !node.children().any(|c| c.kind() == "binding_pattern_kind") {
        return; // an ordinary parameter declares nothing
    }
    let Some(owner) = owner_of(node, package) else {
        return;
    };
    let Some(name) = node
        .children()
        .find(|c| c.kind() == "simple_identifier")
        .map(|c| c.text().to_string())
    else {
        return;
    };
    facts.defs.push(kt_def(
        DefKind::Property,
        name,
        owner,
        DeclSpace::Value,
        visibility_facets(node),
        span_of(node),
    ));
}

/// An enum entry is a constant of the enum, in the same space a `const val`
/// lives in — which is what Kotlin makes it.
fn enum_entry(facts: &mut FileFacts<KtLang>, node: &SgNode, package: &str) {
    let Some(owner) = owner_of(node, package) else {
        return;
    };
    let Some(name) = node
        .children()
        .find(|c| c.kind() == "simple_identifier")
        .map(|c| c.text().to_string())
    else {
        return;
    };
    facts.defs.push(kt_def(
        DefKind::Const,
        name,
        owner,
        DeclSpace::Value,
        DefFacets::EXPORTED.union(DefFacets::STATIC),
        span_of(node),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(kind, owner, name, line)` for every definition, in source order.
    fn census(source: &str) -> Vec<(&'static str, Vec<String>, String, u32)> {
        extract("src/A.kt", source)
            .defs
            .into_iter()
            .map(|d| (d.kind.name(), d.owner, d.name, d.span.line))
            .collect()
    }

    fn facets_of(source: &str, name: &str) -> DefFacets {
        extract("src/A.kt", source)
            .defs
            .into_iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("no definition called {name}"))
            .facets
    }

    #[test]
    fn the_rules_compile() {
        Rules::compile(KOTLIN_RULES).expect("kotlin.yml compiles");
    }

    #[test]
    fn a_file_states_the_package_its_declarations_live_in() {
        let facts = extract("src/A.kt", "package okio.internal\n\nclass A\n");
        assert_eq!(facts.header.package, "okio.internal");
        assert_eq!(facts.defs[0].kind, DefKind::Module);
        assert_eq!(facts.defs[0].name, "okio.internal");
        assert_eq!(facts.defs[1].owner, ["okio.internal"]);
    }

    #[test]
    fn a_file_with_no_package_header_is_in_the_default_package() {
        // The empty string is a container with no name, which is a different
        // fact from a file naming no container — a `.kts` build script is the
        // measured case.
        let facts = extract("build.gradle.kts", "plugins {\n  kotlin(\"jvm\")\n}\n");
        assert_eq!(facts.header.package, "");
        assert_eq!(facts.defs[0].kind, DefKind::Module);
        assert_eq!(facts.defs[0].name, "");
        assert_eq!(facts.defs.len(), 1, "{:?}", facts.defs);
    }

    #[test]
    fn a_package_name_is_read_off_its_segments_and_not_its_text() {
        let facts = extract("src/A.kt", "package okio . internal\n");
        assert_eq!(facts.header.package, "okio.internal");
    }

    #[test]
    fn an_import_is_the_only_reference_kind_this_extractor_emits() {
        let facts = extract(
            "src/A.kt",
            "package okio\n\nimport okio.Buffer\n\n@Throws(IOException::class)\n\
             class A : Sink {\n  fun f() { g(); h.i() }\n}\n",
        );
        assert_eq!(facts.refs.len(), 1, "{:?}", facts.refs);
        let r = &facts.refs[0];
        assert_eq!(r.kind, RefKind::Import);
        assert_eq!(r.raw_target, "okio.Buffer");
        assert_eq!(r.target.segments, ["okio", "Buffer"]);
        assert_eq!(r.target.root, TargetRoot::Name);
        assert!(!r.locally_bound);
        assert!(r.argc.is_none());
        assert_eq!(r.space, DeclSpace::Namespace);
        assert_eq!(
            r.enclosing,
            Some(Encloser {
                path: vec!["okio".to_string()],
                kind: DefKind::Module,
            }),
        );
    }

    #[test]
    fn an_import_clause_and_its_reference_are_paired_by_span() {
        let facts = extract(
            "src/A.kt",
            "package okio\n\nimport a.b.C\nimport a.b.D as E\nimport a.b.*\n",
        );
        assert_eq!(facts.header.imports.len(), facts.refs.len());
        for (spec, r) in facts.header.imports.iter().zip(&facts.refs) {
            assert_eq!(spec.span, r.span, "{}", r.raw_target);
        }
    }

    #[test]
    fn an_alias_is_carried_in_the_raw_target_and_binds_nothing_here() {
        let facts = extract(
            "src/A.kt",
            "package okio\nimport java.nio.file.Path as NioPath\n",
        );
        assert_eq!(facts.refs[0].raw_target, "java.nio.file.Path as NioPath");
        // The alias is not part of what the import *names*.
        assert_eq!(
            facts.refs[0].target.segments,
            ["java", "nio", "file", "Path"]
        );
        assert_eq!(
            facts.header.imports[0].alias.as_deref(),
            Some("NioPath"),
            "the alias is recorded as the site wrote it",
        );
        assert!(!facts.header.imports[0].on_demand);
    }

    #[test]
    fn an_on_demand_import_carries_a_segment_no_identifier_can_spell() {
        let facts = extract("src/A.kt", "package okio\nimport okio.internal.*\n");
        assert_eq!(facts.refs[0].raw_target, "okio.internal.*");
        assert_eq!(facts.refs[0].target.segments, ["okio", "internal", "*"]);
        assert!(facts.header.imports[0].on_demand);
    }

    #[test]
    fn two_imports_of_one_target_under_two_aliases_stay_two_rows() {
        // `raw_target` is the store's dedup key component, so an alias that
        // vanished here would merge two sites into one row.
        let facts = extract(
            "src/A.kt",
            "package okio\nimport java.nio.file.FileSystem as NioFileSystem\n\
             import java.nio.file.FileSystem as JavaNioFileSystem\n",
        );
        assert_ne!(facts.refs[0].raw_target, facts.refs[1].raw_target);
    }

    #[test]
    fn classifiers_carry_the_facet_their_keyword_states() {
        let source = "package p\nclass C\ninterface I\nenum class E { A }\n\
                      annotation class An\nobject O\nabstract class Ab\nprivate class Pr\n\
                      internal class In\n";
        assert!(facets_of(source, "C").contains(DefFacets::EXPORTED));
        assert!(facets_of(source, "I").contains(DefFacets::INTERFACE));
        assert!(facets_of(source, "E").contains(DefFacets::ENUM));
        assert!(facets_of(source, "An").contains(DefFacets::ANNOTATION));
        assert!(facets_of(source, "O").contains(DefFacets::STATIC));
        assert!(facets_of(source, "Ab").contains(DefFacets::ABSTRACT));
        assert!(facets_of(source, "Pr").contains(DefFacets::PRIVATE));
        // `internal` is module-wide, not declaration-local: it is neither
        // exported nor `PRIVATE`, which means "not inherited below here".
        assert_eq!(facets_of(source, "In"), DefFacets::default());
    }

    #[test]
    fn an_unnamed_companion_object_is_declared_under_the_name_kotlin_gives_it() {
        // `import okio.ByteString.Companion.encodeUtf8` spells this name, so
        // the declaration has to be filed under it.
        assert_eq!(
            census("package okio\nclass B {\n  companion object {\n    fun f() {}\n  }\n}\n"),
            [
                ("module", vec![], "okio".to_string(), 1),
                ("type", vec!["okio".into()], "B".to_string(), 2),
                (
                    "type",
                    vec!["okio".into(), "B".into()],
                    "Companion".to_string(),
                    3
                ),
                (
                    "method",
                    vec!["okio".into(), "B".into(), "Companion".into()],
                    "f".to_string(),
                    4,
                ),
            ],
        );
    }

    #[test]
    fn a_named_companion_object_keeps_the_name_it_was_written_with() {
        let defs = census("package okio\nclass B {\n  companion object Key\n}\n");
        assert_eq!(defs[2].2, "Key");
    }

    #[test]
    fn a_typealias_is_a_classifier_and_not_a_callable() {
        let defs = census("package okio\nactual typealias IOException = java.io.IOException\n");
        assert_eq!(defs[1].0, "alias");
        assert_eq!(defs[1].2, "IOException");
        // What it forwards to is a type use, and tier 2 resolves none.
        assert!(
            extract("src/A.kt", "package okio\ntypealias X = Y\n")
                .refs
                .is_empty()
        );
    }

    #[test]
    fn a_top_level_function_is_a_function_and_a_member_one_is_a_method() {
        let defs = census("package okio\nfun top() {}\nclass C {\n  fun member() {}\n}\n");
        assert_eq!(defs[1].0, "function");
        assert_eq!(defs[1].1, ["okio"]);
        assert_eq!(defs[3].0, "method");
        assert_eq!(defs[3].1, ["okio", "C"]);
    }

    #[test]
    fn an_extension_functions_receiver_is_not_part_of_its_name() {
        // `import okio.toUtf8String` names it whatever it extends, so the
        // receiver is not part of the identity either.
        let defs = census("package okio\ninternal fun ByteArray.toUtf8String(): String = \"\"\n");
        assert_eq!(defs[1].2, "toUtf8String");
        assert_eq!(defs[1].1, ["okio"]);
    }

    #[test]
    fn const_val_is_a_constant_and_every_other_val_is_a_property() {
        let defs = census(
            "package okio\nconst val K = 1\nval p = 2\nvar v = 3\n\
             class C {\n  const val M = 4\n}\n",
        );
        assert_eq!(defs[1].0, "const");
        assert_eq!(defs[2].0, "property");
        assert_eq!(defs[3].0, "property");
        assert_eq!(defs[5].0, "const");
        assert_eq!(defs[5].1, ["okio", "C"]);
    }

    #[test]
    fn a_constructor_is_a_node_only_where_the_source_writes_one() {
        let defs = census(
            "package okio\nclass Written(x: Int) {\n  constructor() : this(0)\n}\nclass Implicit\n",
        );
        let ctors: Vec<_> = defs.iter().filter(|d| d.0 == "constructor").collect();
        assert_eq!(ctors.len(), 2, "{defs:?}");
        for c in &ctors {
            assert_eq!(c.1, ["okio", "Written"]);
            assert_eq!(c.2, INIT);
        }
    }

    #[test]
    fn a_val_in_a_primary_constructor_declares_a_property_and_a_bare_parameter_does_not() {
        let defs = census("package okio\nclass C(val kept: Int, dropped: String)\n");
        let names: Vec<&str> = defs.iter().map(|d| d.2.as_str()).collect();
        assert!(names.contains(&"kept"), "{defs:?}");
        assert!(!names.contains(&"dropped"), "{defs:?}");
    }

    #[test]
    fn an_enum_entry_is_a_constant_of_its_enum() {
        let defs = census("package okio\nenum class E {\n  A,\n  B;\n  fun f() {}\n}\n");
        let entries: Vec<_> = defs.iter().filter(|d| d.0 == "const").collect();
        assert_eq!(entries.len(), 2, "{defs:?}");
        assert_eq!(entries[0].1, ["okio", "E"]);
    }

    #[test]
    fn a_declaration_inside_a_function_body_has_no_owner_this_file_states() {
        let defs = census(
            "package okio\nfun outer() {\n  fun inner() {}\n  class Local\n  val x = 1\n}\n",
        );
        assert_eq!(defs.len(), 2, "{defs:?}");
        assert_eq!(defs[1].2, "outer");
    }

    #[test]
    fn an_object_literal_declares_no_nameable_type_so_nothing_in_it_is_a_node() {
        let defs = census(
            "package okio\nclass C {\n  val r = object : Runnable {\n    \
             override fun run() {}\n    val inside = 1\n  }\n}\n",
        );
        let names: Vec<&str> = defs.iter().map(|d| d.2.as_str()).collect();
        assert!(names.contains(&"r"), "{defs:?}");
        assert!(!names.contains(&"run"), "{defs:?}");
        assert!(!names.contains(&"inside"), "{defs:?}");
    }

    #[test]
    fn a_member_declared_in_an_enum_entrys_body_is_on_a_type_with_no_name() {
        let defs = census(
            "package okio\nenum class F {\n  Empty {\n    override fun newBuffer() {}\n  };\n  \
             abstract fun newBuffer()\n}\n",
        );
        let overriding: Vec<_> = defs.iter().filter(|d| d.3 == 4).collect();
        assert!(overriding.is_empty(), "{defs:?}");
        // The abstract member the entries implement is still a node.
        assert!(
            defs.iter().any(|d| d.2 == "newBuffer" && d.3 == 6),
            "{defs:?}"
        );
    }

    #[test]
    fn an_accessor_is_part_of_its_property_rather_than_a_node_of_its_own() {
        let defs = census(
            "package okio\nclass C {\n  var p: Int = 0\n    get() = field\n    \
             set(value) { field = value }\n}\n",
        );
        assert_eq!(defs.len(), 3, "{defs:?}");
        assert_eq!(defs[2].2, "p");
    }

    #[test]
    fn a_declaration_in_an_init_block_is_not_a_node() {
        let defs = census("package okio\nclass C {\n  init { val local = 1 }\n}\n");
        assert_eq!(defs.len(), 2, "{defs:?}");
    }

    #[test]
    fn a_misparsed_class_loses_its_members_rather_than_leaking_them_to_the_top_level() {
        // The measured grammar defect, as a fixture. tree-sitter-kotlin 0.4.1
        // cannot attach a modified primary constructor written on the line
        // after the class header: the class body comes back as a lambda
        // beside the declaration instead of inside it. The members must be
        // dropped — emitting them would mint `okio#utf8()` as a *top-level*
        // function of package `okio`, which is a wrong definition rather than
        // a missing one.
        let source = "package okio\n\nexpect open class ByteString\n\
                      // Trusted internal constructor doesn't clone data.\n\
                      internal constructor(data: ByteArray) : Comparable<ByteString> {\n  \
                      fun utf8(): String\n  \
                      val size: Int\n}\n";
        let defs = census(source);
        for d in &defs {
            assert!(
                d.1.len() <= 1,
                "a member escaped its class: {d:?} in {defs:?}",
            );
            assert!(
                !matches!(d.2.as_str(), "utf8" | "size"),
                "a member was emitted at the top level: {d:?}",
            );
        }
    }

    #[test]
    fn an_import_the_grammar_did_not_understand_states_nothing() {
        // A partial path is exactly the shape that resolves to the wrong
        // package, so a header with an error node emits no reference.
        let facts = extract("src/A.kt", "package okio\nimport okio.\n");
        assert!(facts.refs.len() <= 1, "{:?}", facts.refs);
        for r in &facts.refs {
            assert!(r.target.segments.len() >= 2, "{}", r.raw_target);
        }
    }

    #[test]
    fn records_come_out_in_source_order_with_the_container_first() {
        let facts = extract(
            "src/A.kt",
            "package okio\nimport a.B\nclass C\nimport a.D\nfun f() {}\n",
        );
        assert_eq!(facts.defs[0].kind, DefKind::Module);
        assert!(
            facts
                .defs
                .windows(2)
                .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
            "{:?}",
            facts.defs,
        );
        assert!(
            facts
                .refs
                .windows(2)
                .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
            "{:?}",
            facts.refs,
        );
    }

    #[test]
    fn a_file_that_does_not_parse_still_states_its_container() {
        // tree-sitter is error-tolerant, and a file that does not parse is
        // still a file whose package other files import from.
        let facts = extract("src/A.kt", "package okio\nclass (((\n");
        assert_eq!(facts.defs[0].kind, DefKind::Module);
        assert_eq!(facts.defs[0].name, "okio");
    }
}
