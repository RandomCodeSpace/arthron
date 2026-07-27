//! One PHP file in, records out. Forbidden from linking.
//!
//! # What a tier-2 extractor emits
//!
//! **Definitions and structure, and imports.** Nothing else. The reference
//! kinds this module produces are [`RefKind::Import`] and only that: no call
//! site, no type use, no supertype. A tier-2 language that emitted them
//! un-gated would put references in a rate no tier-2 resolver links, which is
//! tier-1 coverage claimed without tier-1 work.
//!
//! # The three meanings of `use`
//!
//! PHP spells three unrelated things with one keyword, and only position
//! tells them apart:
//!
//! - `use A\B;` at file or namespace-block scope — an **import**, and the
//!   only one of the three this module emits. The grammar calls it a
//!   `namespace_use_declaration`.
//! - `use Helper;` inside a class body — **trait composition**, a
//!   `use_declaration`. It imports no name into the file; it flattens a
//!   trait's members into a class, which is an inheritance fact and so not a
//!   tier-2 reference.
//! - `function ($q) use ($a) {…}` — a closure **capturing** an outer
//!   variable, an `anonymous_function_use_clause`. Not a reference at all.
//!
//! The grammar gives all three different node kinds, so `php.yml` separates
//! them by naming one; the fixture tests hold that separation.
//!
//! # A file's namespace is not its directory, and not a function of the file
//!
//! Every definition carries the namespace it sits in as `owner[0]`, because a
//! file may declare several namespaces with braced blocks — one corpus file
//! declares three — and may declare none, in which case its definitions live
//! in the global namespace, whose name is the empty string.
//!
//! # Known non-claims, recorded rather than left to be rediscovered
//!
//! - **Anonymous classes.** `new class {…}` declares no nameable type, so its
//!   members are not nodes and nothing inside it is emitted. The same
//!   judgement Java makes for an anonymous class body.
//! - **`define('X', …)`.** A function call, not a declaration. Recovering the
//!   constant it declares is a framework-rule question — a string literal
//!   naming a thing — and never the core extractor's.
//! - **`class_alias()`, conditional `class_exists` guards, and `eval`.** Same
//!   answer, same reason.
//! - **Names are case-insensitive in PHP; identities here are not.** PSR-4
//!   maps a name to a path on a case-sensitive filesystem, so a project that
//!   autoloads at all already spells its names consistently. `use guzzlehttp\Client;`
//!   would miss where `use GuzzleHttp\Client;` resolves.

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_php::lang::PhpLang;

use std::sync::OnceLock;

const PHP_RULES: &str = include_str!("../rules/php.yml");

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules::compile(PHP_RULES).expect("embedded php.yml compiles"))
}

/// What one PHP file states about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhpHeader {
    /// The file's repository-relative path.
    pub rel_path: String,
    /// Every namespace this file declares, in source order.
    ///
    /// A `Vec` and not an `Option`, because the file-to-namespace mapping is
    /// not a function: braced `namespace X { … }` blocks let one file
    /// contribute symbols to several. A file that declares none carries one
    /// entry, the empty string — the global namespace is a container with no
    /// name, not the absence of a container.
    pub namespaces: Vec<String>,
}

/// Which of PHP's three symbol tables a `use` clause consults.
///
/// PHP keeps classes, functions and constants in separate tables, so
/// `use A\b;`, `use function A\b;` and `use const A\b;` name three different
/// things. The extractor spells the distinction into the reference's
/// `raw_target` — which is the literal text at the site — because it is what
/// the site says and the resolver has to read it back to pick a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseKind {
    /// A class, interface, trait, enum, or a namespace.
    Class,
    /// `use function` — the function table.
    Function,
    /// `use const` — the constant table.
    Const,
}

impl UseKind {
    /// The keyword this kind writes before the name, `""` for a plain `use`.
    pub fn prefix(self) -> &'static str {
        match self {
            UseKind::Class => "",
            UseKind::Function => "function ",
            UseKind::Const => "const ",
        }
    }

    /// Read the kind back off a reference's `raw_target`.
    ///
    /// The extractor writes the keyword the site wrote; this is the inverse.
    pub fn of(raw_target: &str) -> UseKind {
        if raw_target.starts_with("function ") {
            UseKind::Function
        } else if raw_target.starts_with("const ") {
            UseKind::Const
        } else {
            UseKind::Class
        }
    }
}

/// The PHP extractor. One path and one source string; nothing to link with.
pub struct PhpExtractor;

impl Extractor<PhpLang> for PhpExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<PhpLang> {
        extract(rel_path, source)
    }
}

/// The namespace blocks a file declares, as byte ranges.
///
/// PHP forbids mixing the braced and unbraced forms in one file, so exactly
/// one of two shapes holds: a braced block owns its own byte range, and an
/// unbraced `namespace N;` owns everything up to the next declaration.
struct Blocks {
    spans: Vec<(u32, u32, String)>,
}

impl Blocks {
    fn build(decls: &[SgNode]) -> Blocks {
        let mut spans = Vec::with_capacity(decls.len());
        for (i, node) in decls.iter().enumerate() {
            let span = span_of(node);
            let braced = node.children().any(|c| c.kind() == "compound_statement");
            let end = if braced {
                span.byte_end
            } else {
                decls
                    .get(i + 1)
                    .map_or(u32::MAX, |next| span_of(next).byte_start)
            };
            spans.push((span.byte_start, end, namespace_name(node)));
        }
        Blocks { spans }
    }

    /// The namespace a byte offset sits in. The global namespace — `""` — is
    /// the answer for a file that declares none, and for the code before the
    /// first declaration in a file that does.
    fn at(&self, byte: u32) -> &str {
        self.spans
            .iter()
            .rev()
            .find(|(start, end, _)| *start <= byte && byte < *end)
            .map_or("", |(_, _, name)| name.as_str())
    }
}

/// The name a `namespace_definition` declares: `""` for `namespace { … }`.
fn namespace_name(node: &SgNode) -> String {
    node.children()
        .find(|c| c.kind() == "namespace_name")
        .map(|n| n.text().to_string())
        .unwrap_or_default()
}

/// What owns a declaration, as far as the ancestor chain says.
enum Owner {
    /// Declared directly in a namespace.
    Namespace,
    /// Declared in a named type.
    Type(String),
    /// Declared in an anonymous class, which is not a node — so neither is
    /// anything it declares.
    Anonymous,
}

const TYPE_KINDS: [&str; 4] = [
    "class_declaration",
    "interface_declaration",
    "trait_declaration",
    "enum_declaration",
];

fn owner_of(node: &SgNode) -> Owner {
    for a in node.ancestors() {
        let kind = a.kind();
        if TYPE_KINDS.contains(&&*kind) {
            return match a.field("name") {
                Some(name) => Owner::Type(name.text().to_string()),
                // A type declaration with no name is a recovery artefact:
                // nothing can name it, so nothing inside it is a node.
                None => Owner::Anonymous,
            };
        }
        if kind == "object_creation_expression" {
            return Owner::Anonymous; // `new class { … }`
        }
    }
    Owner::Namespace
}

/// Whether a declaration carries a given modifier node.
fn has_modifier(node: &SgNode, kind: &str) -> bool {
    node.children().any(|c| c.kind() == kind)
}

/// The declared visibility, when one is written.
fn visibility(node: &SgNode) -> Option<String> {
    node.children()
        .find(|c| c.kind() == "visibility_modifier")
        .map(|c| c.text().trim().to_string())
}

fn member_facets(node: &SgNode) -> DefFacets {
    let mut facets = DefFacets::default();
    if has_modifier(node, "static_modifier") {
        facets = facets.union(DefFacets::STATIC);
    }
    if has_modifier(node, "abstract_modifier") {
        facets = facets.union(DefFacets::ABSTRACT);
    }
    if visibility(node).as_deref() == Some("private") {
        facets = facets.union(DefFacets::PRIVATE);
    }
    facets
}

fn php_def(
    kind: DefKind,
    name: String,
    owner: Vec<String>,
    facets: DefFacets,
    span: Span,
) -> Definition {
    Definition {
        kind,
        name,
        owner,
        // PHP declares classes, functions and constants in three tables, and
        // the FQN grammar — not this field — is what keeps them apart, so
        // every declaration lands in one space here. `space` discriminates a
        // *reference*'s table, which is where PHP needs it.
        space: DeclSpace::Value,
        facets,
        // PHP has no overloading: one name is one declaration, so nothing
        // discriminates by arity.
        params: None,
        span,
    }
}

/// Extract all facts from one PHP source file.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<PhpLang> {
    let tree = SourceTree::parse_php(source);
    let matches = tree.matches(rules());
    let namespace_decls: Vec<SgNode> = matches
        .iter()
        .filter(|(_, n)| n.kind() == "namespace_definition")
        .map(|(_, n)| n.clone())
        .collect();
    let blocks = Blocks::build(&namespace_decls);

    let mut header = PhpHeader {
        rel_path: rel_path.to_string(),
        namespaces: Vec::new(),
    };
    let mut defs: Vec<Definition> = Vec::new();
    let mut refs: Vec<Reference> = Vec::new();

    // A file that declares no namespace still has a container: the global
    // one. Emitting it is what keeps every file's definitions reachable from
    // a container node, and what gives a reference in such a file an edge
    // source.
    if namespace_decls.is_empty() {
        header.namespaces.push(String::new());
        defs.push(php_def(
            DefKind::Module,
            String::new(),
            Vec::new(),
            DefFacets::default(),
            Span {
                byte_start: 0,
                byte_end: source.len().min(u32::MAX as usize) as u32,
                line: 1,
            },
        ));
    }

    for (_, node) in &matches {
        let span = span_of(node);
        let ns = blocks.at(span.byte_start).to_string();
        match &*node.kind() {
            "namespace_definition" => {
                let name = namespace_name(node);
                header.namespaces.push(name.clone());
                defs.push(php_def(
                    DefKind::Module,
                    name,
                    Vec::new(),
                    DefFacets::default(),
                    span,
                ));
            }
            "namespace_use_declaration" => {
                for import in use_clauses(node) {
                    refs.push(Reference {
                        kind: RefKind::Import,
                        space: match import.kind {
                            UseKind::Class => DeclSpace::Type,
                            UseKind::Function | UseKind::Const => DeclSpace::Value,
                        },
                        raw_target: import.raw_target(),
                        target: RefTarget {
                            root: TargetRoot::Name,
                            segments: import
                                .name
                                .split('\\')
                                .map(str::to_string)
                                .collect::<Vec<_>>(),
                        },
                        // Structurally false: an import names an absolute
                        // path in a namespace, and no block binds one.
                        // `LocalBinding` does not apply at tier 2 — there is
                        // no expression-level reference to be bound.
                        locally_bound: false,
                        argc: None,
                        enclosing: Some(Encloser {
                            path: vec![ns.clone()],
                            kind: DefKind::Module,
                        }),
                        span: import.span,
                    });
                }
            }
            kind if TYPE_KINDS.contains(&kind) => {
                let Some(name) = node.field("name") else {
                    continue; // recovery artefact: nothing can name it
                };
                let mut facets = DefFacets::default();
                if kind == "interface_declaration" {
                    facets = facets.union(DefFacets::INTERFACE);
                }
                if kind == "enum_declaration" {
                    facets = facets.union(DefFacets::ENUM);
                }
                if has_modifier(node, "abstract_modifier") {
                    facets = facets.union(DefFacets::ABSTRACT);
                }
                // A trait is a `Type` with no facet of its own: there is no
                // `TRAIT` bit, and `DefFacets` is core rather than this
                // track's to widen.
                defs.push(php_def(
                    DefKind::Type,
                    name.text().to_string(),
                    vec![ns],
                    facets,
                    span,
                ));
            }
            // Only ever a namespace-level function: a method is a
            // `method_declaration`. One written inside another function body
            // still declares its name in the namespace when it runs.
            "function_definition" => {
                let Some(name) = node.field("name") else {
                    continue;
                };
                defs.push(php_def(
                    DefKind::Function,
                    name.text().to_string(),
                    vec![ns],
                    DefFacets::default(),
                    span,
                ));
            }
            "method_declaration" => {
                let Owner::Type(class) = owner_of(node) else {
                    continue; // an anonymous class declares no node
                };
                let Some(name) = node.field("name") else {
                    continue;
                };
                let name = name.text().to_string();
                let owner = vec![ns, class];
                if name == "__construct" {
                    // Constructor property promotion: a parameter with a
                    // visibility modifier declares a property of the class.
                    for param in promoted_properties(node) {
                        defs.push(php_def(
                            DefKind::Field,
                            param.0,
                            owner.clone(),
                            param.1,
                            param.2,
                        ));
                    }
                }
                defs.push(php_def(
                    DefKind::Method,
                    name,
                    owner,
                    member_facets(node),
                    span,
                ));
            }
            "property_declaration" => {
                let Owner::Type(class) = owner_of(node) else {
                    continue;
                };
                let facets = member_facets(node);
                for element in node.children().filter(|c| c.kind() == "property_element") {
                    let Some(name) = variable_name(&element) else {
                        continue;
                    };
                    defs.push(php_def(
                        DefKind::Field,
                        name,
                        vec![ns.clone(), class.clone()],
                        facets,
                        span_of(&element),
                    ));
                }
            }
            "const_declaration" => {
                let owner = match owner_of(node) {
                    Owner::Namespace => vec![ns],
                    Owner::Type(class) => vec![ns, class],
                    Owner::Anonymous => continue,
                };
                let facets = member_facets(node);
                for element in node.children().filter(|c| c.kind() == "const_element") {
                    let Some(name) = element.children().find(|c| c.kind() == "name") else {
                        continue;
                    };
                    defs.push(php_def(
                        DefKind::Const,
                        name.text().to_string(),
                        owner.clone(),
                        facets,
                        span_of(&element),
                    ));
                }
            }
            "enum_case" => {
                let Owner::Type(class) = owner_of(node) else {
                    continue;
                };
                let Some(name) = node.children().find(|c| c.kind() == "name") else {
                    continue;
                };
                // An enum case is a constant of the enum, in the same table a
                // class constant lives in — which is exactly what PHP makes
                // it.
                defs.push(php_def(
                    DefKind::Const,
                    name.text().to_string(),
                    vec![ns, class],
                    DefFacets::default(),
                    span,
                ));
            }
            _ => {}
        }
    }

    FileFacts { header, defs, refs }
}

/// `(name, facets, span)` for every promoted property a constructor declares.
fn promoted_properties(method: &SgNode) -> Vec<(String, DefFacets, Span)> {
    let Some(params) = method.field("parameters") else {
        return Vec::new();
    };
    params
        .children()
        .filter(|c| c.kind() == "property_promotion_parameter")
        .filter_map(|param| {
            let name = variable_name(&param)?;
            Some((name, member_facets(&param), span_of(&param)))
        })
        .collect()
}

/// The identifier inside a `$name` variable, without the sigil.
fn variable_name(node: &SgNode) -> Option<String> {
    let var = node.children().find(|c| c.kind() == "variable_name")?;
    var.children()
        .find(|c| c.kind() == "name")
        .map(|n| n.text().to_string())
}

/// One `use` clause: what it names, in which table, under which alias.
struct Import {
    kind: UseKind,
    /// The fully-qualified name, with no leading `\` and with a group's
    /// prefix already applied.
    name: String,
    alias: Option<String>,
    span: Span,
}

impl Import {
    /// The literal text at the site, canonicalised.
    ///
    /// The keyword the site wrote, the name it named, and the alias it bound.
    /// A group clause carries the prefix its group states and a leading `\`
    /// is dropped, because `use \A\B;` and `use A\B;` are the same import —
    /// a `use` target is absolute whether or not it is spelled that way, and
    /// two rows for one import would be two rows for one fact.
    fn raw_target(&self) -> String {
        let mut out = String::with_capacity(self.name.len() + 16);
        out.push_str(self.kind.prefix());
        out.push_str(&self.name);
        if let Some(alias) = &self.alias {
            out.push_str(" as ");
            out.push_str(alias);
        }
        out
    }
}

/// Every clause one `namespace_use_declaration` states.
fn use_clauses(decl: &SgNode) -> Vec<Import> {
    let mut decl_kind: Option<UseKind> = None;
    let mut prefix: Option<String> = None;
    let mut group: Option<SgNode> = None;
    let mut clauses: Vec<SgNode> = Vec::new();
    for child in decl.children() {
        match &*child.kind() {
            "function" => decl_kind = Some(UseKind::Function),
            "const" => decl_kind = Some(UseKind::Const),
            "namespace_name" => prefix = Some(child.text().to_string()),
            "namespace_use_group" => group = Some(child.clone()),
            "namespace_use_clause" => clauses.push(child.clone()),
            _ => {}
        }
    }
    if let Some(group) = &group {
        clauses = group
            .children()
            .filter(|c| c.kind() == "namespace_use_clause")
            .collect();
    } else {
        // The grammar hangs `use function A\b, A\c;`'s keyword on the *first*
        // clause alone. PHP applies it to every clause in the declaration, so
        // reading each clause's own keyword would look `A\c` up in the class
        // table.
        prefix = None;
        decl_kind = decl_kind.or_else(|| clauses.first().and_then(clause_kind));
    }
    let kind = decl_kind.unwrap_or(UseKind::Class);
    clauses
        .iter()
        .filter_map(|clause| {
            let (name, alias) = clause_name(clause)?;
            let name = match &prefix {
                Some(p) => format!("{p}\\{name}"),
                None => name,
            };
            Some(Import {
                kind,
                name: name.trim_start_matches('\\').to_string(),
                alias,
                span: span_of(clause),
            })
        })
        .collect()
}

fn clause_kind(clause: &SgNode) -> Option<UseKind> {
    clause.children().find_map(|c| match &*c.kind() {
        "function" => Some(UseKind::Function),
        "const" => Some(UseKind::Const),
        _ => None,
    })
}

/// `(name as written, alias)` for one clause.
///
/// The alias is the `name` child that follows the `as` token — a clause that
/// renames has two `name` children and only their order tells them apart.
fn clause_name(clause: &SgNode) -> Option<(String, Option<String>)> {
    let mut name: Option<String> = None;
    let mut alias: Option<String> = None;
    let mut after_as = false;
    for child in clause.children() {
        match &*child.kind() {
            "as" => after_as = true,
            "qualified_name" if !after_as => name = Some(child.text().to_string()),
            "name" => {
                if after_as {
                    alias = Some(child.text().to_string());
                } else if name.is_none() {
                    name = Some(child.text().to_string());
                }
            }
            _ => {}
        }
    }
    Some((name?, alias))
}
