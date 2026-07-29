//! Rust extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/rust.yml`) select nodes by kind; this
//! module interprets their fields. Rust is a **tier-2** language here, and the
//! whole of what that means is visible in this file: the only references it
//! emits are module and import references — `use`, `mod`, `extern crate`. No
//! call site and no type use becomes a reference, because arthron makes no
//! verified claim about either for Rust, and a tier-2 track that emitted them
//! un-gated would report tier-1 coverage it has not measured.
//!
//! # The owner chain, and why module segments carry a marker
//!
//! [`crate::lang::Resolver::def_fqn`] receives one owner chain and has to
//! know where the *module* nesting ends and the *type* nesting begins: a
//! function in `mod tests` is `…::tests#helper`, while a method on `Foo` is
//! `…#Foo.helper`, and the two are different nodes. Nothing in
//! [`Definition`] carries that split, so the extractor marks each inline
//! module segment with a leading `::` — a sequence no Rust identifier can
//! contain. The marker never leaves this track: every FQN is composed by
//! `def_fqn`, which strips it, and no owner chain is ever stored.
//!
//! A span would have been the other way to carry the split, and it is the
//! wrong one: [`Encloser::as_definition`] zeroes the span precisely so that
//! no FQN is composed of a fact an unrelated edit moves, so an encloser's
//! chain would silently lose its modules.
//!
//! # Known under-counts, recorded rather than rediscovered
//!
//! - **Struct and union fields are not definitions.** Nothing at tier 2 can
//!   name one — a `use` path never reaches a field — and Go, the closest
//!   tier-1 analogue, does not emit them either.
//! - **Items declared inside a function body are not nameable**, so they are
//!   not emitted. A `use` *statement* inside a body still is a reference: it
//!   names a module, and which block it sits in changes nothing about that.
//! - **A glob re-export enumerates nothing.** `pub use x::*` is a reference
//!   to `x`, which resolves, and it binds no alias here, because the names it
//!   forwards are a fact about `x` rather than about this file. A later `use`
//!   of one of those names therefore misses honestly instead of resolving
//!   through a set nobody built. Nothing is written for the glob *itself*
//!   either, which is why the resolver cannot answer such a miss with
//!   [`crate::UnresolvedReason::WildcardImport`]: with no probeable trace of
//!   the glob, "a name a glob forwards" and "a name that is absent" are the
//!   same observation.
//! - **A non-`pub` module-scope `use` binds nothing.** Only a `pub` one binds
//!   an alias, because only a `pub` one is a name another module may write a
//!   path to. It is still a binding *inside* its own module, though, and a
//!   `use super::Name` from a child module really does reach it — so those
//!   references miss. Both of the measured corpus's
//!   [`crate::UnresolvedReason::NoMatchingDefinition`] rows are exactly this
//!   shape. Binding one unconditionally is not the fix: an alias node is
//!   reachable by FQN from anywhere, so a private import binding published as
//!   one would let unrelated modules resolve through a name Rust does not
//!   give them — a wrong edge in place of a missing one. Visibility has to
//!   reach the resolver first.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_rust::lang::RsLang;

/// The embedded Rust extraction rules.
const RUST_RULES: &str = include_str!("../rules/rust.yml");

/// The marker an inline module segment carries in an owner chain.
///
/// `::` cannot appear in a Rust identifier, so a marked segment can never be
/// mistaken for a type name. See the module docs for why the split has to be
/// carried at all.
pub const MODULE_MARK: &str = "::";

/// One name a `pub use` or `pub extern crate` re-exports, and the path it
/// forwards to.
///
/// Paired with its [`Definition`] by `byte_start`, because
/// [`crate::lang::Resolver::def_alias_targets`] receives the definition and
/// the header and nothing else — and in the definition phase a definition's
/// span is the real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReExport {
    /// Byte offset of the leaf that binds the name.
    pub byte_start: u32,
    /// The inline module chain the re-export sits in, unmarked.
    pub module: Vec<String>,
    /// The path it forwards to, as the reference records it.
    pub target: RefTarget,
}

/// Per-file Rust facts only the Rust resolver reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RsHeader {
    /// Repo-relative, `/`-separated path of the file. A Rust module's name is
    /// a fact about where its file sits, so the resolver needs the path and
    /// the core must not be the layer that turns one into a module.
    pub rel_path: String,
    /// Every `use` declaration in the file, counted. One declaration yields
    /// one reference per leaf of its tree, so this is a floor on the
    /// references and never their number.
    pub use_decls: usize,
    /// What this file re-exports, in source order.
    pub reexports: Vec<ReExport>,
}

/// The Rust extractor. Stateless.
pub struct RsExtractor;

impl Extractor<RsLang> for RsExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<RsLang> {
        extract(rel_path, source)
    }
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules::compile(RUST_RULES).expect("embedded rust.yml compiles"))
}

/// Extract one Rust file.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<RsLang> {
    let tree = SourceTree::parse_rust(source);
    let mut header = RsHeader {
        rel_path: rel_path.to_string(),
        use_decls: 0,
        reexports: Vec::new(),
    };
    // The file's own module, first in the list and synthetic: no `mod`
    // keyword in this file declares it, and `def_fqn` reads the facet to tell
    // it from an inline `mod x { … }` written at file scope. `container_fqn`
    // takes the first `Module` definition, so this must stay at index 0.
    let mut defs = vec![Definition {
        kind: DefKind::Module,
        name: file_module_name(rel_path),
        owner: Vec::new(),
        space: DeclSpace::Namespace,
        facets: DefFacets::SYNTHETIC.union(DefFacets::EXPORTED),
        params: None,
        span: Span {
            byte_start: 0,
            byte_end: source.len() as u32,
            line: 1,
        },
    }];
    let mut refs: Vec<Reference> = Vec::new();

    for (id, node) in tree.matches(rules()) {
        match id {
            "use" => {
                header.use_decls += 1;
                use_declaration(&node, &mut header, &mut defs, &mut refs);
            }
            "extern-crate" => extern_crate(&node, &mut header, &mut defs, &mut refs),
            "def-mod" => module_item(&node, &mut defs, &mut refs),
            _ => {
                if let Some(def) = definition(id, &node) {
                    defs.push(def);
                }
            }
        }
    }

    // Source order, and the file's own module kept at the head of it. Rule
    // order is what `matches` returns, and it is not the order a reader — or
    // `container_fqn` — expects.
    defs[1..].sort_by_key(|d| (d.span.byte_start, d.span.byte_end));
    refs.sort_by_key(|r| (r.span.byte_start, r.span.byte_end));
    header.reexports.sort_by_key(|e| e.byte_start);

    FileFacts { header, defs, refs }
}

/// The short name of the module a file declares.
///
/// `foo.rs` is `foo` and `foo/mod.rs` is `foo`; a crate root takes its file
/// stem, which is what `lib.rs` or `main.rs` is called before anything reads
/// a manifest. Only the *name* is decided here — where the module sits is a
/// manifest fact, and the resolver owns it.
fn file_module_name(rel_path: &str) -> String {
    let stem = rel_path.rsplit_once('/').map_or(rel_path, |(_, f)| f);
    let stem = stem.strip_suffix(".rs").unwrap_or(stem);
    if stem != "mod" {
        return stem.to_string();
    }
    match rel_path.rsplit_once('/') {
        Some((dir, _)) => dir.rsplit_once('/').map_or(dir, |(_, d)| d).to_string(),
        None => stem.to_string(),
    }
}

// -- context -----------------------------------------------------------------

/// Whether a node sits inside a function body, where nothing it declares is
/// nameable from anywhere else.
fn inside_body(node: &SgNode) -> bool {
    node.ancestors().any(|a| a.kind() == "block")
}

/// The inline module chain a node sits in, outermost first and unmarked.
fn module_chain(node: &SgNode) -> Vec<String> {
    let mut out: Vec<String> = node
        .ancestors()
        .filter(|a| a.kind() == "mod_item" && a.field("body").is_some())
        .filter_map(|a| a.field("name").map(|n| n.text().to_string()))
        .collect();
    out.reverse();
    out
}

/// The name of the type an `impl` block is for, and the trait it implements.
///
/// A generic, referenced or qualified type yields its first type identifier —
/// `Iter<'a>` is `Iter`, `fmt::Display` is `Display` — because that is the
/// name a path can reach. An `impl` for a type with no identifier at all,
/// `impl Trait for &str`, names nothing this track can place, so its members
/// are not emitted rather than filed under a guess.
fn impl_owner(node: &SgNode) -> Option<Vec<String>> {
    let ty = type_name(&node.field("type")?)?;
    // The trait comes second, so two traits' identically named methods on one
    // type stay two nodes instead of silently merging into one.
    match node.field("trait").and_then(|t| type_name(&t)) {
        Some(tr) => Some(vec![ty, tr]),
        None => Some(vec![ty]),
    }
}

fn type_name(node: &SgNode) -> Option<String> {
    if node.kind() == "type_identifier" {
        return Some(node.text().to_string());
    }
    node.dfs()
        .find(|n| n.kind() == "type_identifier")
        .map(|n| n.text().to_string())
}

/// The owner chain of everything *above* a node: marked module segments
/// outermost, then the type chain.
///
/// `None` when the node sits in a function body, or under an `impl` whose
/// type this track cannot name — in both cases nothing outside can name what
/// is being declared, so it is not a node.
fn owner_chain(node: &SgNode) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for a in node.ancestors() {
        match &*a.kind() {
            "block" => return None,
            "mod_item" if a.field("body").is_some() => {
                let name = a.field("name")?.text().to_string();
                out.push(format!("{MODULE_MARK}{name}"));
            }
            "impl_item" => out.extend(impl_owner(&a)?.into_iter().rev()),
            "trait_item" => out.push(a.field("name")?.text().to_string()),
            "enum_item" | "struct_item" | "union_item" => {
                out.push(a.field("name")?.text().to_string());
            }
            _ => {}
        }
    }
    out.reverse();
    Some(out)
}

/// The nearest *nameable* enclosing definition of a reference site.
///
/// A `use` at module scope has none, and the driver then sources its edge at
/// the file's own module — which is exactly where an import belongs.
fn enclosing_definition(node: &SgNode) -> Option<Encloser> {
    for a in node.ancestors() {
        let (name, kind) = match &*a.kind() {
            "function_item" | "function_signature_item" => {
                let name = a.field("name")?.text().to_string();
                let owner = owner_chain(&a)?;
                let kind = if owner.last().is_some_and(|s| !s.starts_with(MODULE_MARK)) {
                    DefKind::Method
                } else {
                    DefKind::Function
                };
                (name, kind)
            }
            "mod_item" if a.field("body").is_some() => (
                // Marked, so `def_fqn` reads it as the module it is. The
                // marker is stripped there and never stored.
                format!("{MODULE_MARK}{}", a.field("name")?.text()),
                DefKind::Module,
            ),
            _ => continue,
        };
        let mut path = owner_chain(&a)?;
        path.push(name);
        return Some(Encloser { path, kind });
    }
    None
}

// -- definitions -------------------------------------------------------------

/// Whether a declaration carries any `pub` visibility.
fn is_public(node: &SgNode) -> bool {
    node.children().any(|c| c.kind() == "visibility_modifier")
}

fn facets(node: &SgNode, extra: DefFacets) -> DefFacets {
    if is_public(node) {
        extra.union(DefFacets::EXPORTED)
    } else {
        extra
    }
}

/// One definition from a matched declaration node, or `None` when nothing
/// outside the declaration can name it.
fn definition(rule: &str, node: &SgNode) -> Option<Definition> {
    if inside_body(node) {
        return None; // an item in a function body is not a node
    }
    let owner = owner_chain(node)?;
    let name = node.field("name")?.text().to_string();
    let in_type = owner.last().is_some_and(|s| !s.starts_with(MODULE_MARK));
    let (kind, space, extra) = match (rule, &*node.kind()) {
        ("def-function", "function_signature_item") => {
            (DefKind::Method, DeclSpace::Value, DefFacets::ABSTRACT)
        }
        ("def-function", _) if in_type => (DefKind::Method, DeclSpace::Value, DefFacets::default()),
        ("def-function", _) => (DefKind::Function, DeclSpace::Value, DefFacets::default()),
        ("def-type", "trait_item") => (DefKind::Type, DeclSpace::Type, DefFacets::INTERFACE),
        ("def-type", "enum_item") => (DefKind::Type, DeclSpace::Type, DefFacets::ENUM),
        ("def-type", "associated_type") => (DefKind::Type, DeclSpace::Type, DefFacets::ABSTRACT),
        ("def-type", _) => (DefKind::Type, DeclSpace::Type, DefFacets::default()),
        // A variant is the one thing a `use` path reaches *through* a type:
        // `use crate::m::E::A` names it, and nothing else in Rust's item
        // grammar behaves that way.
        ("def-variant", _) => (DefKind::Constructor, DeclSpace::Value, DefFacets::default()),
        ("def-const", "static_item") => (DefKind::Var, DeclSpace::Value, DefFacets::STATIC),
        ("def-const", _) => (DefKind::Const, DeclSpace::Value, DefFacets::default()),
        // A macro is textually scoped rather than path-scoped, so this is the
        // declaration site and not a claim that any invocation reaches it.
        ("def-macro", _) => (DefKind::Function, DeclSpace::Value, DefFacets::SYNTHETIC),
        _ => return None,
    };
    Some(Definition {
        kind,
        name,
        owner,
        space,
        facets: facets(node, extra),
        params: None,
        span: span_of(node),
    })
}

/// A `mod` item: an inline `mod x { … }` declares a module, and a bodyless
/// `mod x;` names the one its own file declares.
///
/// Only the inline form is a definition. Emitting a node for `mod x;` too
/// would make every one of the corpus's 80 module declarations resolve to
/// something this file minted, which measures nothing — the file `x.rs` is
/// what declares that module, and a `mod x;` whose file is absent under every
/// configuration must miss.
fn module_item(node: &SgNode, defs: &mut Vec<Definition>, refs: &mut Vec<Reference>) {
    if inside_body(node) {
        return;
    }
    let Some(owner) = owner_chain(node) else {
        return;
    };
    let Some(name) = node.field("name").map(|n| n.text().to_string()) else {
        return;
    };
    if node.field("body").is_some() {
        defs.push(Definition {
            kind: DefKind::Module,
            name,
            owner,
            space: DeclSpace::Namespace,
            facets: facets(node, DefFacets::default()),
            params: None,
            span: span_of(node),
        });
        return;
    }
    refs.push(Reference {
        kind: RefKind::Import,
        space: DeclSpace::Namespace,
        raw_target: format!("mod {name}"),
        target: RefTarget {
            root: TargetRoot::This {
                qualifier: module_chain(node),
            },
            // No leading `self`: a `mod` declaration is the one relative site
            // that can *only* name a module, and that shape is what the
            // resolver reads to decide the reason a miss carries. A `use
            // self::x;` writes the keyword and keeps it.
            segments: vec![name],
        },
        locally_bound: false,
        argc: None,
        arg_types: None,
        enclosing: enclosing_definition(node),
        span: span_of(node),
    });
}

// -- imports -----------------------------------------------------------------

/// `extern crate x;` and `pub extern crate x as y;`.
fn extern_crate(
    node: &SgNode,
    header: &mut RsHeader,
    defs: &mut Vec<Definition>,
    refs: &mut Vec<Reference>,
) {
    let Some(name) = node.field("name").map(|n| n.text().to_string()) else {
        return;
    };
    let target = RefTarget {
        root: TargetRoot::Name,
        segments: vec![name.clone()],
    };
    let span = span_of(node);
    if let Some(alias) = node.field("alias").map(|a| a.text().to_string())
        && is_public(node)
        && let Some(owner) = owner_chain(node)
    {
        bind_alias(header, defs, owner, alias, target.clone(), span.byte_start);
    }
    refs.push(Reference {
        kind: RefKind::Import,
        space: DeclSpace::Namespace,
        raw_target: format!("extern crate {name}"),
        target,
        locally_bound: false,
        argc: None,
        arg_types: None,
        enclosing: enclosing_definition(node),
        span,
    });
}

/// One `use` declaration: one reference per leaf of its tree, and one alias
/// definition per leaf a `pub` use binds.
fn use_declaration(
    node: &SgNode,
    header: &mut RsHeader,
    defs: &mut Vec<Definition>,
    refs: &mut Vec<Reference>,
) {
    let Some(argument) = node.field("argument") else {
        return;
    };
    let public = is_public(node);
    let owner = owner_chain(node);
    let enclosing = enclosing_definition(node);
    let chain = module_chain(node);
    let mut leaves = Vec::new();
    walk_use(&argument, &[], &mut leaves);
    for leaf in leaves {
        let target = target_of(&chain, &leaf.segments);
        let raw = raw_of(&leaf);
        if public && let (Some(name), Some(owner)) = (leaf.binding, owner.clone()) {
            bind_alias(header, defs, owner, name, target.clone(), leaf.byte_start);
        }
        refs.push(Reference {
            kind: RefKind::Import,
            space: DeclSpace::Namespace,
            raw_target: raw,
            target,
            locally_bound: false,
            argc: None,
            arg_types: None,
            enclosing: enclosing.clone(),
            span: Span {
                byte_start: leaf.byte_start,
                byte_end: leaf.byte_end,
                line: leaf.line,
            },
        });
    }
}

/// Record one re-exported name: an alias definition plus the path it forwards
/// to, paired by byte offset.
fn bind_alias(
    header: &mut RsHeader,
    defs: &mut Vec<Definition>,
    owner: Vec<String>,
    name: String,
    target: RefTarget,
    byte_start: u32,
) {
    let module: Vec<String> = owner
        .iter()
        .filter_map(|s| s.strip_prefix(MODULE_MARK).map(str::to_string))
        .collect();
    header.reexports.push(ReExport {
        byte_start,
        module,
        target,
    });
    defs.push(Definition {
        kind: DefKind::Alias,
        name,
        owner,
        // A `use` binding occupies whichever namespaces its target does.
        // Nothing branches on this, and recording one is not a claim it is
        // the only one.
        space: DeclSpace::Value,
        facets: DefFacets::EXPORTED,
        params: None,
        span: Span {
            byte_start,
            byte_end: byte_start,
            line: 0,
        },
    });
}

/// One leaf of a `use` tree: the full path it names, and the name it binds.
struct Leaf {
    /// The path segments, a trailing `*` included. `*` is not an identifier,
    /// so carrying the glob in the shape rather than in a flag keeps the
    /// resolver reading structure instead of text.
    segments: Vec<String>,
    /// The name this leaf binds in the enclosing module, or `None` for a glob
    /// — which binds a set this file cannot enumerate.
    binding: Option<String>,
    byte_start: u32,
    byte_end: u32,
    line: u32,
}

/// Expand a `use` tree into its leaves, carrying the prefix down.
fn walk_use(node: &SgNode, prefix: &[String], out: &mut Vec<Leaf>) {
    let span = span_of(node);
    match &*node.kind() {
        "use_list" => {
            for child in node.children() {
                if matches!(
                    &*child.kind(),
                    "scoped_identifier"
                        | "identifier"
                        | "crate"
                        | "self"
                        | "super"
                        | "use_as_clause"
                        | "use_wildcard"
                        | "use_list"
                        | "scoped_use_list"
                ) {
                    walk_use(&child, prefix, out);
                }
            }
        }
        "scoped_use_list" => {
            let mut inner = prefix.to_vec();
            if let Some(path) = node.field("path") {
                path_segments(&path, &mut inner);
            }
            if let Some(list) = node.field("list") {
                walk_use(&list, &inner, out);
            }
        }
        "use_as_clause" => {
            let mut segments = prefix.to_vec();
            if let Some(path) = node.field("path") {
                path_segments(&path, &mut segments);
            }
            // `use a::b::{self as z}` renames the module itself, so the path
            // stops one short of the alias.
            if segments.last().is_some_and(|s| s == "self") {
                segments.pop();
            }
            out.push(Leaf {
                segments,
                binding: node.field("alias").map(|a| a.text().to_string()),
                byte_start: span.byte_start,
                byte_end: span.byte_end,
                line: span.line,
            });
        }
        "use_wildcard" => {
            let mut segments = prefix.to_vec();
            if let Some(first) = node.children().next()
                && first.kind() != "*"
            {
                path_segments(&first, &mut segments);
            }
            segments.push("*".to_string());
            out.push(Leaf {
                segments,
                binding: None,
                byte_start: span.byte_start,
                byte_end: span.byte_end,
                line: span.line,
            });
        }
        "self" => {
            // `use a::{self}` names `a` itself and binds its last segment.
            let segments = prefix.to_vec();
            let binding = segments.last().cloned();
            out.push(Leaf {
                segments,
                binding,
                byte_start: span.byte_start,
                byte_end: span.byte_end,
                line: span.line,
            });
        }
        _ => {
            let mut segments = prefix.to_vec();
            path_segments(node, &mut segments);
            let binding = segments.last().cloned();
            out.push(Leaf {
                segments,
                binding,
                byte_start: span.byte_start,
                byte_end: span.byte_end,
                line: span.line,
            });
        }
    }
}

/// Flatten a path node into its segments.
///
/// A leading `::` is dropped: since the 2018 edition `::x` and `x` name the
/// same thing at the root of a `use`, and every manifest in the measured
/// corpus is edition 2024.
fn path_segments(node: &SgNode, out: &mut Vec<String>) {
    match &*node.kind() {
        "scoped_identifier" => {
            if let Some(path) = node.field("path") {
                path_segments(&path, out);
            }
            if let Some(name) = node.field("name") {
                out.push(name.text().to_string());
            }
        }
        "identifier" | "type_identifier" | "crate" | "self" | "super" | "metavariable" => {
            out.push(node.text().to_string());
        }
        _ => {}
    }
}

/// Classify a path's root, carrying the site's inline module chain with it.
///
/// `self` and `super` are relative to the module the site sits in, and an
/// inline `mod` block moves that — so the chain has to travel with the
/// reference rather than be recovered from its span.
fn target_of(chain: &[String], segments: &[String]) -> RefTarget {
    let root = match segments.first().map(String::as_str) {
        Some("self") => TargetRoot::This {
            qualifier: chain.to_vec(),
        },
        Some("super") => TargetRoot::Super {
            qualifier: chain.to_vec(),
        },
        _ => TargetRoot::Name,
    };
    RefTarget {
        root,
        segments: segments.to_vec(),
    }
}

fn raw_of(leaf: &Leaf) -> String {
    leaf.segments.join("::")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(source: &str) -> FileFacts<RsLang> {
        extract("src/lib.rs", source)
    }

    fn raws(source: &str) -> Vec<String> {
        facts(source)
            .refs
            .iter()
            .map(|r| r.raw_target.clone())
            .collect()
    }

    fn names(source: &str) -> Vec<(DefKind, String, Vec<String>)> {
        facts(source)
            .defs
            .iter()
            .map(|d| (d.kind, d.name.clone(), d.owner.clone()))
            .collect()
    }

    #[test]
    fn a_file_declares_its_own_module_first_and_synthetically() {
        let facts = facts("fn f() {}\n");
        let module = &facts.defs[0];
        assert_eq!(module.kind, DefKind::Module);
        assert_eq!(module.name, "lib");
        assert!(module.facets.contains(DefFacets::SYNTHETIC));
        // `foo/mod.rs` declares module `foo`, not module `mod`.
        assert_eq!(file_module_name("a/b/foo/mod.rs"), "foo");
        assert_eq!(file_module_name("a/b/foo.rs"), "foo");
        assert_eq!(file_module_name("build.rs"), "build");
    }

    #[test]
    fn a_use_list_becomes_one_reference_per_leaf() {
        assert_eq!(
            raws("use std::io::{self, Write, Read};\n"),
            ["std::io", "std::io::Write", "std::io::Read"]
        );
    }

    #[test]
    fn a_nested_use_list_carries_its_prefix_all_the_way_down() {
        assert_eq!(
            raws("use crate::{a::B, c::{D, E}};\n"),
            ["crate::a::B", "crate::c::D", "crate::c::E"]
        );
    }

    #[test]
    fn a_rename_names_the_path_and_binds_the_alias() {
        let facts = facts("pub use crate::a::B as C;\n");
        assert_eq!(facts.refs[0].raw_target, "crate::a::B");
        let alias = facts
            .defs
            .iter()
            .find(|d| d.kind == DefKind::Alias)
            .expect("a public rename binds an alias");
        assert_eq!(alias.name, "C");
        // `use a::{self as z}` renames the module, so the path stops short.
        assert_eq!(raws("use crate::a::{self as z};\n"), ["crate::a"]);
    }

    #[test]
    fn a_glob_names_the_module_it_globs_and_binds_nothing() {
        let facts = facts("pub use crate::a::*;\n");
        assert_eq!(facts.refs[0].raw_target, "crate::a::*");
        assert_eq!(facts.refs[0].target.segments, ["crate", "a", "*"]);
        assert!(
            !facts.defs.iter().any(|d| d.kind == DefKind::Alias),
            "a glob re-export enumerates nothing, so it binds no alias",
        );
    }

    #[test]
    fn a_leading_colon_colon_is_the_same_path_since_2018() {
        assert_eq!(raws("use ::std::fmt;\n"), ["std::fmt"]);
    }

    #[test]
    fn self_and_super_carry_the_sites_inline_module_chain() {
        let facts = facts("mod outer { mod inner { use self::x; use super::super::y; } }\n");
        let by_raw = |raw: &str| {
            facts
                .refs
                .iter()
                .find(|r| r.raw_target == raw)
                .unwrap_or_else(|| panic!("no reference `{raw}`"))
                .target
                .clone()
        };
        assert_eq!(
            by_raw("self::x").root,
            TargetRoot::This {
                qualifier: vec!["outer".into(), "inner".into()],
            }
        );
        assert_eq!(
            by_raw("super::super::y").root,
            TargetRoot::Super {
                qualifier: vec!["outer".into(), "inner".into()],
            }
        );
        assert_eq!(by_raw("super::super::y").segments, ["super", "super", "y"]);
    }

    #[test]
    fn a_bodyless_mod_is_a_reference_and_an_inline_mod_is_a_definition() {
        let facts = facts("mod a;\nmod b { }\n");
        assert_eq!(
            facts
                .refs
                .iter()
                .map(|r| r.raw_target.clone())
                .collect::<Vec<_>>(),
            ["mod a"]
        );
        // No `self` keyword: the shape is what says "this can only name a
        // module", and `use self::a;` beside it would carry one.
        assert_eq!(facts.refs[0].target.segments, ["a"]);
        assert!(
            facts
                .defs
                .iter()
                .any(|d| d.kind == DefKind::Module && d.name == "b"),
            "an inline module is declared here",
        );
        assert!(
            !facts
                .defs
                .iter()
                .any(|d| d.kind == DefKind::Module && d.name == "a"),
            "`mod a;` names the module `a.rs` declares; minting one here would \
             make every module declaration resolve to itself",
        );
    }

    #[test]
    fn an_inline_module_segment_is_marked_and_a_type_segment_is_not() {
        let got = names("mod t { struct S; impl S { fn m(&self) {} } }\n");
        assert!(got.contains(&(DefKind::Module, "t".into(), vec![])));
        assert!(got.contains(&(DefKind::Type, "S".into(), vec!["::t".into()])));
        assert!(got.contains(&(DefKind::Method, "m".into(), vec!["::t".into(), "S".into()])));
    }

    #[test]
    fn a_trait_impl_files_its_members_under_the_trait_as_well_as_the_type() {
        let got = names(
            "impl Display for S { fn fmt(&self) {} }\nimpl Debug for S { fn fmt(&self) {} }\n",
        );
        assert!(got.contains(&(
            DefKind::Method,
            "fmt".into(),
            vec!["S".into(), "Display".into()]
        )));
        assert!(got.contains(&(
            DefKind::Method,
            "fmt".into(),
            vec!["S".into(), "Debug".into()]
        )));
    }

    #[test]
    fn every_item_kind_becomes_a_definition_of_its_own_shape() {
        let got = names(
            "pub struct S;\npub enum E { A, B }\npub union U { a: u8 }\n\
             pub trait T { type O; const K: u8; fn m(&self); }\n\
             pub type Al = S;\npub const C: u8 = 0;\nstatic X: u8 = 0;\n\
             macro_rules! mac { () => {} }\npub fn f() {}\n",
        );
        for want in [
            (DefKind::Type, "S", vec![]),
            (DefKind::Type, "E", vec![]),
            (DefKind::Constructor, "A", vec!["E".to_string()]),
            (DefKind::Constructor, "B", vec!["E".to_string()]),
            (DefKind::Type, "U", vec![]),
            (DefKind::Type, "T", vec![]),
            (DefKind::Type, "O", vec!["T".to_string()]),
            (DefKind::Const, "K", vec!["T".to_string()]),
            (DefKind::Method, "m", vec!["T".to_string()]),
            (DefKind::Type, "Al", vec![]),
            (DefKind::Const, "C", vec![]),
            (DefKind::Var, "X", vec![]),
            (DefKind::Function, "mac", vec![]),
            (DefKind::Function, "f", vec![]),
        ] {
            assert!(
                got.contains(&(want.0, want.1.to_string(), want.2.clone())),
                "{want:?} is missing from {got:?}",
            );
        }
    }

    #[test]
    fn an_item_in_a_function_body_is_not_a_node_but_its_use_is_a_reference() {
        let facts = facts("fn f() { use crate::a::B; struct Local; }\n");
        assert!(
            !facts.defs.iter().any(|d| d.name == "Local"),
            "nothing outside the body can name it",
        );
        assert_eq!(facts.refs[0].raw_target, "crate::a::B");
        assert_eq!(
            facts.refs[0].enclosing.as_ref().map(|e| e.kind),
            Some(DefKind::Function),
        );
    }

    #[test]
    fn an_extern_crate_names_a_crate_and_a_public_one_binds_an_alias() {
        let facts = facts("pub extern crate grep_cli as cli;\nextern crate test;\n");
        assert_eq!(
            facts
                .refs
                .iter()
                .map(|r| r.raw_target.clone())
                .collect::<Vec<_>>(),
            ["extern crate grep_cli", "extern crate test"]
        );
        assert_eq!(facts.refs[0].target.segments, ["grep_cli"]);
        let alias = facts
            .defs
            .iter()
            .find(|d| d.kind == DefKind::Alias)
            .expect("a public extern crate binds an alias");
        assert_eq!(alias.name, "cli");
    }

    #[test]
    fn a_private_use_binds_no_alias() {
        // A private import is not reachable from another module, so minting a
        // node for it would resolve paths Rust rejects.
        assert!(
            !facts("use crate::a::B;\n")
                .defs
                .iter()
                .any(|d| d.kind == DefKind::Alias),
        );
    }

    #[test]
    fn no_call_and_no_type_use_is_ever_a_reference() {
        // The tier-2 contract, asserted rather than assumed: emitting either
        // un-gated would report coverage nobody measured.
        let facts = facts("fn f(x: Other) -> Thing { g(x); h::i(x) }\n");
        assert!(
            facts.refs.is_empty(),
            "tier 2 emits import and module references only: {:?}",
            facts.refs.iter().map(|r| &r.raw_target).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn every_reference_is_an_import_and_none_is_locally_bound() {
        let facts = facts("use std::io;\nmod a;\nextern crate test;\nmod b { use super::io; }\n");
        assert!(!facts.refs.is_empty());
        for r in &facts.refs {
            assert_eq!(r.kind, RefKind::Import);
            assert_eq!(r.space, DeclSpace::Namespace);
            assert!(
                !r.locally_bound,
                "tier 2 has no expression-level reference, so nothing is locally bound",
            );
            assert_eq!(r.argc, None);
        }
    }

    #[test]
    fn a_use_inside_an_inline_module_is_sourced_at_that_module() {
        let facts = facts("mod t { use crate::a::B; }\n");
        let encloser = facts.refs[0].enclosing.as_ref().expect("an encloser");
        assert_eq!(encloser.kind, DefKind::Module);
        // Marked, so `def_fqn` reads it as a module rather than a type.
        assert_eq!(encloser.path, ["::t"]);
    }

    #[test]
    fn records_come_back_in_source_order_with_the_module_at_the_head() {
        let facts = facts("use a::b;\nfn f() {}\nmod m { }\nuse c::d;\n");
        assert_eq!(facts.defs[0].kind, DefKind::Module);
        let starts: Vec<u32> = facts.defs[1..].iter().map(|d| d.span.byte_start).collect();
        assert!(starts.windows(2).all(|w| w[0] <= w[1]), "{starts:?}");
        let starts: Vec<u32> = facts.refs.iter().map(|r| r.span.byte_start).collect();
        assert!(starts.windows(2).all(|w| w[0] <= w[1]), "{starts:?}");
    }

    #[test]
    fn a_broken_file_still_yields_records() {
        // tree-sitter is error-tolerant, and `extract` is total: a file that
        // does not parse is still read for what it does say.
        let facts = facts("use std::io;\nfn f( {\n");
        assert!(facts.refs.iter().any(|r| r.raw_target == "std::io"));
    }
}
