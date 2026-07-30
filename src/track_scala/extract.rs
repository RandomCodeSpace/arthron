//! Scala extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/scala.yml`) select nodes by kind; this
//! module interprets their fields. Scala is a **tier-2** language here, and
//! the whole of what that means is visible in this file: the only references
//! it emits are `import` references. No call site, no type use, no `extends`
//! clause becomes a reference, because arthron makes no verified claim about
//! any of them for Scala, and a tier-2 track that emitted them un-gated would
//! report tier-1 coverage it has not measured.
//!
//! # One reference per selector, not per line
//!
//! `import a.{B, C}` names two things and produces two references; `import
//! a._` names one — the container `a` — and produces one. A wildcard's
//! forwarded set is never enumerated and never guessed, which is why its
//! reference stops at the prefix rather than inventing a name per member.
//!
//! Each reference is paired with an [`ImportSpec`] in the header by span, so
//! the resolver can read the *form* a clause was written in without the core
//! learning what a Scala import is.
//!
//! # Continuation imports
//!
//! A top-level `import` is in scope from where it is written to the end of
//! the compilation unit, so a later import path may *start* at a name an
//! earlier one bound — `import upickletest.TestUtil` and then, inside a test
//! body, `import TestUtil._`. Those bindings are recorded in the header as
//! [`ImportBinding`]s, in source order, and only for top-level clauses: a
//! nested import binds inside its own block, and claiming a binding reaches
//! further than it does is how a resolver invents a container.
//!
//! # The package is in the source, so the owner chain is absolute
//!
//! A Scala file states its package; nothing about its directory, its source
//! root or its build target enters the name. So every [`Definition::owner`]
//! this extractor emits starts at the file's package and is absolute, and
//! [`crate::lang::Resolver::def_fqn`] needs no fact from outside the file to
//! compose an FQN. Container segments carry
//! [`crate::track_scala::lang::CONTAINER_MARK`]; see that module for the
//! grammar and for what the mark costs.
//!
//! A `package a.b` clause declares **both** `a` and `a.b`: `a.b` cannot exist
//! without `a`, and a path written from elsewhere walks through `a` to reach
//! it. So one definition is emitted per prefix, and the file's own innermost
//! package is emitted first — [`crate::pipeline`] reads the first `Module`
//! definition as the container a file-scope reference is sourced at.
//!
//! # Known under-counts, recorded rather than left to be rediscovered
//!
//! Each is a real declaration this extractor does not emit. None may be
//! closed by widening a bucket, and none is closed by guessing.
//!
//! - **A `case class`'s synthesized companion.** Scala mints `object Foo`
//!   beside `case class Foo`, with an `apply` and an `unapply` nobody wrote.
//!   Emitting it would make `import p.Foo` resolve to a synthesized container
//!   in preference to the class actually written there — a *different* target
//!   for the same site, which is worse than a missing member.
//! - **`export`.** Scala 3's `export a.b.*` declares forwarder members; it is
//!   neither an import nor a declaration this file writes, and treating it as
//!   an import would put a re-export in a column that counts imports. One
//!   site in the measured corpus.
//! - **A `val` bound by a pattern.** `val (a, b) = t` and `val Some(x) = o`
//!   really do declare names, through an extractor whose result this build
//!   does not evaluate. Only a plain identifier pattern is a definition.
//! - **An anonymous `given`.** `given Int = 4` declares an instance with no
//!   name a path can write, so no node is invented for it.
//! - **Anything a term declares.** A class, an object or a `def` written
//!   inside a method body, a `val` initialiser, or the body of `new T { … }`
//!   is not nameable from anywhere else, so it is not a node. An `import`
//!   inside one still is a reference: it names a container, and which block
//!   it sits in changes nothing about that.
//! - **Overload arity.** `def f(a: Int)` and `def f(a: String)` in one class
//!   share an FQN and become one node with two declaration sites. Tier 2 has
//!   no call site to discriminate at, so [`Definition::params`] is left
//!   `None` rather than carrying a shape nothing reads.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_scala::lang::{
    ScalaLang, clause_segments, is_container, mark, mark_qualifier, unmark,
};

/// The embedded Scala extraction rules.
const SCALA_RULES: &str = include_str!("../rules/scala.yml");

/// How one import selector was written.
///
/// The distinction the resolver never needs and a reader of the measurement
/// always does: a wildcard names a container and forwards a set nobody here
/// enumerates, while a named selector names one declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportForm {
    /// `import a.b.C`, or `C` inside a selector list.
    Named,
    /// `import a.{C => D}` or `import a.{C as D}` — one name under another,
    /// including the `=> _` form, which names `C` in order to hide it.
    Renamed,
    /// `import a._` or `import a.*` — every member, none enumerated.
    Wildcard,
    /// `import a.given` — every given instance, none enumerated. Scala 3
    /// imports these by *type* rather than by name, so there is not even a
    /// name here to fail to enumerate.
    GivenWildcard,
}

impl ImportForm {
    /// The form's stable name, for a census a test can read.
    pub fn name(self) -> &'static str {
        match self {
            ImportForm::Named => "named",
            ImportForm::Renamed => "renamed",
            ImportForm::Wildcard => "wildcard",
            ImportForm::GivenWildcard => "given-wildcard",
        }
    }
}

/// One import selector: how it was written plus where it sits.
///
/// Every `ImportSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    /// How the selector was written.
    pub form: ImportForm,
    /// Where the selector sits — the whole declaration when it has only one.
    pub span: Span,
}

/// One name a top-level import binds, and the path it binds to.
///
/// In scope from `byte_start` to the end of the file, which is exactly what
/// the language says about a top-level import — so a resolver may offer it to
/// any later site without deciding how far a block reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBinding {
    /// The simple name brought into scope: the last path segment, or the new
    /// name when the selector renames.
    pub name: String,
    /// The path it names, as written at the site.
    pub segments: Vec<String>,
    /// Where the binding begins.
    pub byte_start: u32,
}

/// Per-file Scala facts only the Scala resolver reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScalaHeader {
    /// Repo-relative, `/`-separated path of the file. Carried for
    /// diagnostics; nothing in Scala's identity depends on it.
    pub rel_path: String,
    /// The container chain the file's top-level `package` clauses spell,
    /// outermost first, as **marked** segments — see
    /// [`crate::track_scala::lang`] for the two marks and why a qualified
    /// clause's intermediate segments open no scope. Empty for the unnamed
    /// root package.
    pub package: Vec<String>,
    /// Every import selector, in source order.
    pub imports: Vec<ImportSpec>,
    /// Every name a top-level import binds, in source order.
    pub bindings: Vec<ImportBinding>,
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules::compile(SCALA_RULES).expect("embedded scala.yml compiles"))
}

/// Whether a node is a direct child of the compilation unit.
fn top_level(node: &SgNode) -> bool {
    node.ancestors().all(|a| a.kind() == "compilation_unit")
}

/// Ancestor kinds that declare a *term*. Nothing below one is nameable from
/// outside, whether it sits in a block, in a `val` initialiser, or in the
/// body of an anonymous `new T { … }`.
fn opaque(kind: &str) -> bool {
    matches!(
        kind,
        "block"
            | "instance_expression"
            | "lambda_expression"
            | "case_clause"
            | "function_definition"
            | "function_declaration"
            | "val_definition"
            | "val_declaration"
            | "var_definition"
            | "var_declaration"
            | "given_definition"
    )
}

/// The absolute owner chain of everything *above* a node: the file's package,
/// then every enclosing container and type, outermost first.
///
/// `None` when the node sits under something a term declares — in which case
/// nothing outside can name what is being declared, so it is not a node.
fn owner_chain(node: &SgNode, package: &[String]) -> Option<Vec<String>> {
    let mut inner: Vec<String> = Vec::new();
    for a in node.ancestors() {
        let kind = a.kind();
        if opaque(&kind) {
            return None;
        }
        match &*kind {
            // A bodyless clause is the file's own package, already in
            // `package`; a braced one is a container this node sits inside.
            "package_clause" if a.field("body").is_some() => {
                for segment in clause_segments(&a.field("name")?.text()).into_iter().rev() {
                    inner.push(segment);
                }
            }
            "package_object" | "object_definition" => {
                inner.push(mark(&a.field("name")?.text()));
            }
            "class_definition" | "trait_definition" | "enum_definition" => {
                inner.push(a.field("name")?.text().to_string());
            }
            _ => {}
        }
    }
    inner.reverse();
    let mut out: Vec<String> = package.to_vec();
    out.extend(inner);
    Some(out)
}

/// The name and kind a node contributes to an owner path, and whether it is a
/// container. `None` for a node that declares nothing nameable.
fn declared(node: &SgNode) -> Option<(Vec<String>, DefKind, bool)> {
    let one = |name: String, kind: DefKind, container: bool| Some((vec![name], kind, container));
    match &*node.kind() {
        "package_clause" if node.field("body").is_some() => Some((
            node.field("name")?
                .text()
                .split('.')
                .map(str::to_string)
                .collect(),
            DefKind::Module,
            true,
        )),
        "package_object" | "object_definition" => one(
            node.field("name")?.text().to_string(),
            DefKind::Module,
            true,
        ),
        "class_definition" | "trait_definition" | "enum_definition" | "type_definition" => {
            one(node.field("name")?.text().to_string(), DefKind::Type, false)
        }
        "simple_enum_case" | "full_enum_case" => one(
            node.field("name")?.text().to_string(),
            DefKind::Constructor,
            false,
        ),
        "function_definition" | "function_declaration" => one(
            node.field("name")?.text().to_string(),
            DefKind::Function,
            false,
        ),
        "val_declaration" => one(
            node.field("name")?.text().to_string(),
            DefKind::Const,
            false,
        ),
        "var_declaration" => one(node.field("name")?.text().to_string(), DefKind::Var, false),
        "given_definition" => one(
            node.field("name")?.text().to_string(),
            DefKind::Const,
            false,
        ),
        "val_definition" | "var_definition" => {
            let pattern = node.field("pattern")?;
            if pattern.kind() != "identifier" {
                return None; // a destructuring pattern; see the module docs
            }
            let kind = if node.kind() == "val_definition" {
                DefKind::Const
            } else {
                DefKind::Var
            };
            one(pattern.text().to_string(), kind, false)
        }
        _ => None,
    }
}

/// The nearest *nameable* enclosing definition of a reference site.
///
/// An import at file scope has none, and the driver then sources its edge at
/// the file's package — which is exactly where a file-scope import belongs.
fn enclosing_definition(node: &SgNode, package: &[String]) -> Option<Encloser> {
    for a in node.ancestors() {
        let Some((names, kind, container)) = declared(&a) else {
            continue;
        };
        let mut path = owner_chain(&a, package)?;
        // A container's own name is marked here too, so that a resolver
        // reading the marked prefix of this path back sees the whole chain a
        // relative import is looked up in. `def_fqn` strips it.
        let kind = match kind {
            DefKind::Function if path.last().is_some_and(|s| !is_container(s)) => DefKind::Method,
            other => other,
        };
        let last = names.len();
        for (at, name) in names.into_iter().enumerate() {
            path.push(match (container, at + 1 == last) {
                (false, _) => name,
                (true, true) => mark(&name),
                // An intermediate segment of a braced `package a.b { … }`
                // clause: a container, and not a scope.
                (true, false) => mark_qualifier(&name),
            });
        }
        return Some(Encloser { path, kind });
    }
    None
}

/// Whether a declaration carries a `private` or `protected` modifier.
fn access(node: &SgNode) -> DefFacets {
    let Some(modifiers) = node.children().find(|c| c.kind() == "modifiers") else {
        return DefFacets::EXPORTED; // Scala's default is public
    };
    let text = modifiers.text();
    if text.contains("private") {
        DefFacets::PRIVATE
    } else if text.contains("protected") {
        // Not exported, and not `PRIVATE` either: a protected member *is*
        // inherited, which is the one thing that bit distinguishes.
        DefFacets::default()
    } else {
        DefFacets::EXPORTED
    }
}

/// One definition, with the fields every Scala declaration shares.
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

/// The `Definition` one prefix of a marked container chain declares.
///
/// An empty chain is the root package: a file that writes no `package`
/// clause still has a container, and [`crate::track_scala::resolve`] names it
/// `_root_`.
fn package_def(chain: &[String], span: Span) -> Definition {
    let (name, owner) = match chain.split_last() {
        Some((last, rest)) => (unmark(last).to_string(), rest.to_vec()),
        None => (String::new(), Vec::new()),
    };
    def(
        DefKind::Module,
        name,
        owner,
        DeclSpace::Namespace,
        DefFacets::SYNTHETIC.union(DefFacets::EXPORTED),
        span,
    )
}

/// Extract one Scala file. The whole of the extractor's public surface.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<ScalaLang> {
    let tree = SourceTree::parse_scala(source);
    let matched = tree.matches(rules());

    // The file's own package, first, because everything below is named
    // relative to it. Two top-level `package a` / `package b` clauses are
    // Scala's way of writing `package a.b`, so they concatenate.
    let mut package: Vec<String> = Vec::new();
    let mut package_span = Span {
        byte_start: 0,
        byte_end: source.len() as u32,
        line: 1,
    };
    let mut clauses = 0usize;
    for (id, node) in &matched {
        if *id != "package-clause" || node.field("body").is_some() || !top_level(node) {
            continue;
        }
        if let Some(name) = node.field("name") {
            package.extend(clause_segments(&name.text()));
            if clauses == 0 {
                package_span = span_of(node);
            }
            clauses += 1;
        }
    }

    let mut header = ScalaHeader {
        rel_path: rel_path.to_string(),
        package: package.clone(),
        imports: Vec::new(),
        bindings: Vec::new(),
    };
    // The file's innermost package is at index 0: `container_fqn` reads the
    // first `Module` definition as the container a file-scope import is
    // sourced at. Its proper prefixes follow, because `upickle.core` cannot
    // exist without `upickle` and a path from elsewhere walks through it.
    let mut defs = vec![package_def(&package, package_span)];
    for cut in (1..package.len()).rev() {
        defs.push(package_def(&package[..cut], package_span));
    }
    let mut refs: Vec<Reference> = Vec::new();

    for (id, node) in &matched {
        match *id {
            "import" => import_declaration(node, &package, &mut header, &mut refs),
            "package-clause" => {
                if node.field("body").is_some() {
                    braced_package(node, &package, &mut defs);
                }
            }
            _ => {
                if let Some(d) = definition(node, &package) {
                    defs.push(d);
                }
            }
        }
    }

    // Source order, with the file's own package kept at the head of it. Rule
    // order is what `matches` returns, and it is neither what a reader nor
    // what `container_fqn` expects.
    defs[1..].sort_by_key(|d| (d.span.byte_start, d.span.byte_end));
    refs.sort_by_key(|r| (r.span.byte_start, r.span.byte_end));
    header
        .imports
        .sort_by_key(|i| (i.span.byte_start, i.span.byte_end));
    header.bindings.sort_by_key(|b| b.byte_start);

    FileFacts { header, defs, refs }
}

/// `package a.b { … }`: one definition per segment, all containers.
fn braced_package(node: &SgNode, package: &[String], defs: &mut Vec<Definition>) {
    let Some(owner) = owner_chain(node, package) else {
        return;
    };
    let Some(name) = node.field("name") else {
        return;
    };
    let segments = clause_segments(&name.text());
    let span = span_of(node);
    for at in 0..segments.len() {
        let mut owner = owner.clone();
        owner.extend(segments[..at].iter().cloned());
        defs.push(def(
            DefKind::Module,
            unmark(&segments[at]).to_string(),
            owner,
            DeclSpace::Namespace,
            DefFacets::SYNTHETIC.union(DefFacets::EXPORTED),
            span,
        ));
    }
}

/// One matched declaration, or `None` when nothing outside it can name it.
fn definition(node: &SgNode, package: &[String]) -> Option<Definition> {
    // The container flag is for the *owner chain*, which only
    // `enclosing_definition` builds; a definition's own kind already says it.
    let (names, kind, _container) = declared(node)?;
    let owner = owner_chain(node, package)?;
    let name = names.into_iter().next()?;
    let (kind, space) = match kind {
        // A `def` whose owner chain ends in a container is a free function;
        // one whose owner ends in a class, trait or enum is a method.
        DefKind::Function if owner.last().is_some_and(|s| !is_container(s)) => {
            (DefKind::Method, DeclSpace::Value)
        }
        DefKind::Function => (DefKind::Function, DeclSpace::Value),
        DefKind::Module => (DefKind::Module, space_of_container(node)),
        DefKind::Type => (DefKind::Type, DeclSpace::Type),
        other => (other, DeclSpace::Value),
    };
    let extra = match &*node.kind() {
        "trait_definition" => DefFacets::INTERFACE,
        "enum_definition" => DefFacets::ENUM,
        "function_declaration" | "val_declaration" | "var_declaration" => DefFacets::ABSTRACT,
        "type_definition" if !node.children().any(|c| c.kind() == "=") => DefFacets::ABSTRACT,
        "class_definition" if node.children().any(|c| c.kind() == "case") => DefFacets::RECORD,
        _ => DefFacets::default(),
    };
    Some(def(
        kind,
        name,
        owner,
        space,
        access(node).union(extra),
        span_of(node),
    ))
}

/// Which declaration table a container lands in.
///
/// A **package** is a namespace; an **object** is a term, and Scala really
/// does let `object Foo` and `class Foo` be written side by side. Recording
/// the difference is what lets [`crate::lang::Resolver::mergeable`] tell a
/// package every file in it reopens from a declaration two build
/// configurations each write once.
fn space_of_container(node: &SgNode) -> DeclSpace {
    match &*node.kind() {
        "object_definition" => DeclSpace::Value,
        _ => DeclSpace::Namespace,
    }
}

/// One `import` declaration: one reference per selector.
fn import_declaration(
    node: &SgNode,
    package: &[String],
    header: &mut ScalaHeader,
    refs: &mut Vec<Reference>,
) {
    let items: Vec<SgNode> = node
        .children()
        .filter(|c| !matches!(&*c.kind(), "import" | "." | "export"))
        .collect();
    let Some((last, prefix_nodes)) = items.split_last() else {
        return; // `import` with nothing after it is not an import site
    };
    let prefix: Vec<String> = prefix_nodes
        .iter()
        .filter(|n| n.kind() == "identifier")
        .map(|n| n.text().to_string())
        .collect();
    let enclosing = enclosing_definition(node, package);
    let whole = span_of(node);
    // Only a top-level import is in scope for the rest of the file.
    let binds = top_level(node);

    match &*last.kind() {
        // `import a.b.C`: every identifier is a path segment and the last one
        // is the name bound.
        "identifier" => {
            let name = last.text().to_string();
            let mut segments = prefix;
            segments.push(name.clone());
            let raw = segments.join(".");
            emit(
                header,
                refs,
                segments,
                ImportForm::Named,
                raw,
                whole,
                &enclosing,
                binds.then_some(name),
            );
        }
        // `import a.b._`, `import a.b.*`, `import a.b.given`. A wildcard
        // binds a set this build never enumerates, so it binds no name here.
        "namespace_wildcard" => {
            let form = wildcard_form(last);
            let raw = spell(&prefix, &last.text());
            emit(header, refs, prefix, form, raw, whole, &enclosing, None);
        }
        "namespace_selectors" => {
            for selector in last.children() {
                let (segments, form, raw, bound) = match &*selector.kind() {
                    "identifier" => {
                        let name = selector.text().to_string();
                        let raw = spell(&prefix, &name);
                        let mut segments = prefix.clone();
                        segments.push(name.clone());
                        (segments, ImportForm::Named, raw, Some(name))
                    }
                    "arrow_renamed_identifier" | "as_renamed_identifier" => {
                        let Some(name) = selector.field("name") else {
                            continue;
                        };
                        let raw = spell(&prefix, &squeeze(&selector.text()));
                        let mut segments = prefix.clone();
                        segments.push(name.text().to_string());
                        // `X => _` hides `X` rather than binding it.
                        let bound = renamed_to(&selector).filter(|n| n != "_");
                        (segments, ImportForm::Renamed, raw, bound)
                    }
                    "namespace_wildcard" => {
                        let raw = spell(&prefix, &selector.text());
                        (prefix.clone(), wildcard_form(&selector), raw, None)
                    }
                    _ => continue, // punctuation
                };
                emit(
                    header,
                    refs,
                    segments,
                    form,
                    raw,
                    span_of(&selector),
                    &enclosing,
                    bound.filter(|_| binds),
                );
            }
        }
        _ => {}
    }
}

/// The new name a renaming selector introduces: the last identifier or
/// wildcard in `X => Y` / `X as Y`.
fn renamed_to(selector: &SgNode) -> Option<String> {
    selector
        .children()
        .filter(|c| matches!(&*c.kind(), "identifier" | "wildcard" | "_"))
        .last()
        .map(|c| c.text().to_string())
}

/// `_` and `*` import every member; `given` imports every given instance.
fn wildcard_form(node: &SgNode) -> ImportForm {
    if node.text().trim() == "given" {
        ImportForm::GivenWildcard
    } else {
        ImportForm::Wildcard
    }
}

/// A path prefix and a tail, spelled the way the site writes them.
fn spell(prefix: &[String], tail: &str) -> String {
    if prefix.is_empty() {
        return tail.to_string();
    }
    format!("{}.{tail}", prefix.join("."))
}

/// Collapse the whitespace inside a selector so a renamed import keys the
/// same row however it was laid out.
fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Record one selector: its clause in the header, the name it brings into
/// scope beside that, and its reference beside both.
#[allow(clippy::too_many_arguments)]
fn emit(
    header: &mut ScalaHeader,
    refs: &mut Vec<Reference>,
    segments: Vec<String>,
    form: ImportForm,
    raw_target: String,
    span: Span,
    enclosing: &Option<Encloser>,
    binds: Option<String>,
) {
    header.imports.push(ImportSpec { form, span });
    if let Some(name) = binds {
        header.bindings.push(ImportBinding {
            name,
            segments: segments.clone(),
            byte_start: span.byte_start,
        });
    }
    refs.push(Reference {
        kind: RefKind::Import,
        space: DeclSpace::Namespace,
        raw_target,
        target: RefTarget {
            root: TargetRoot::Name,
            segments,
        },
        // Tier 2 emits no expression-level reference, so nothing here can
        // name a local: `LocalBinding` does not apply to this track.
        locally_bound: false,
        argc: None,
        arg_types: None,
        enclosing: enclosing.clone(),
        span,
    });
}

/// The Scala extractor, as the driver holds it.
pub struct ScalaExtractor;

impl Extractor<ScalaLang> for ScalaExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<ScalaLang> {
        extract(rel_path, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track_scala::lang::CONTAINER_MARK;

    fn facts(source: &str) -> FileFacts<ScalaLang> {
        extract("src/Example.scala", source)
    }

    /// `(kind, name, owner)` for every definition, in emitted order.
    fn defs(source: &str) -> Vec<(DefKind, String, Vec<String>)> {
        facts(source)
            .defs
            .iter()
            .map(|d| (d.kind, d.name.clone(), d.owner.clone()))
            .collect()
    }

    fn raws(source: &str) -> Vec<String> {
        facts(source)
            .refs
            .iter()
            .map(|r| r.raw_target.clone())
            .collect()
    }

    fn forms(source: &str) -> Vec<&'static str> {
        facts(source)
            .header
            .imports
            .iter()
            .map(|i| i.form.name())
            .collect()
    }

    #[test]
    fn the_rules_compile() {
        Rules::compile(SCALA_RULES).expect("scala.yml compiles");
    }

    #[test]
    fn a_package_clause_declares_every_prefix_innermost_first() {
        let got = defs("package upickle.core\n");
        assert_eq!(
            got,
            [
                (
                    DefKind::Module,
                    "core".to_string(),
                    vec!["..upickle".to_string()]
                ),
                (DefKind::Module, "upickle".to_string(), vec![]),
            ],
        );
    }

    #[test]
    fn two_package_clauses_are_one_package() {
        // `package a` followed by `package b` is Scala's way of writing
        // `package a.b`; reading them as two separate packages would file
        // every definition in the file under the wrong container.
        let got = defs("package a\npackage b\nclass C\n");
        assert_eq!(
            got[0],
            (DefKind::Module, "b".to_string(), vec![".a".to_string()])
        );
        let c = got.iter().find(|d| d.1 == "C").expect("C");
        assert_eq!(c.2, [".a".to_string(), ".b".to_string()]);
    }

    #[test]
    fn a_file_with_no_package_clause_still_declares_its_container() {
        // Two of the measured corpus's files are exactly this shape: a
        // `package object` at the top of a file that writes no `package`
        // clause of its own.
        let got = defs("package object ujson {\n  def read(): Int = 1\n}\n");
        assert_eq!(got[0], (DefKind::Module, String::new(), vec![]));
        assert_eq!(got[1], (DefKind::Module, "ujson".to_string(), vec![]));
        assert_eq!(
            got[2],
            (
                DefKind::Function,
                "read".to_string(),
                vec![".ujson".to_string()]
            ),
        );
    }

    #[test]
    fn the_files_own_package_is_the_first_module_definition() {
        // `crate::pipeline::container_fqn` reads the first `Module`
        // definition as the container a file-scope import is sourced at, so
        // this ordering is load-bearing and not cosmetic.
        for source in [
            "package a.b.c\nclass C\n",
            "package object p { def f(): Int = 1 }\n",
            "class Bare\n",
        ] {
            let facts = facts(source);
            assert_eq!(facts.defs[0].kind, DefKind::Module);
            assert!(facts.defs[0].owner.iter().all(|s| is_container(s)));
        }
    }

    #[test]
    fn a_companion_pair_is_two_definitions_that_differ_in_kind() {
        let got = defs("package p\nclass Foo\nobject Foo\n");
        assert!(got.contains(&(DefKind::Type, "Foo".to_string(), vec![".p".to_string()])));
        assert!(got.contains(&(DefKind::Module, "Foo".to_string(), vec![".p".to_string()])));
    }

    #[test]
    fn an_object_is_a_container_and_a_class_is_not() {
        let got = defs("package p\nobject O {\n  class C {\n    object Deep\n  }\n}\n");
        let c = got.iter().find(|d| d.1 == "C").expect("C");
        assert_eq!(c.2, [".p".to_string(), ".O".to_string()]);
        // `Deep` sits under a class, so the container run has already ended
        // and the segment above it is unmarked.
        let deep = got.iter().find(|d| d.1 == "Deep").expect("Deep");
        assert_eq!(
            deep.2,
            [".p".to_string(), ".O".to_string(), "C".to_string()]
        );
    }

    #[test]
    fn a_def_in_an_object_is_a_function_and_one_in_a_class_is_a_method() {
        let got =
            defs("package p\nobject O { def free(): Int = 1 }\nclass C { def bound(): Int = 2 }\n");
        let free = got.iter().find(|d| d.1 == "free").expect("free");
        assert_eq!(free.0, DefKind::Function);
        let bound = got.iter().find(|d| d.1 == "bound").expect("bound");
        assert_eq!(bound.0, DefKind::Method);
    }

    #[test]
    fn the_declaration_kinds_scala_writes() {
        let got = defs(
            "package p\n\
             class Klass\n\
             case class Rec(a: Int)\n\
             trait Trt { def abs(): Unit\n  val absVal: Int }\n\
             enum Colour { case Red, Green }\n\
             object Obj { val v = 1\n  var w = 2\n  type Alias = Int\n  given g: Int = 3 }\n",
        );
        let by_name: std::collections::BTreeMap<&str, DefKind> =
            got.iter().map(|d| (d.1.as_str(), d.0)).collect();
        assert_eq!(by_name["Klass"], DefKind::Type);
        assert_eq!(by_name["Rec"], DefKind::Type);
        assert_eq!(by_name["Trt"], DefKind::Type);
        assert_eq!(by_name["abs"], DefKind::Method);
        assert_eq!(by_name["absVal"], DefKind::Const);
        assert_eq!(by_name["Colour"], DefKind::Type);
        assert_eq!(by_name["Red"], DefKind::Constructor);
        assert_eq!(by_name["Green"], DefKind::Constructor);
        assert_eq!(by_name["Obj"], DefKind::Module);
        assert_eq!(by_name["v"], DefKind::Const);
        assert_eq!(by_name["w"], DefKind::Var);
        assert_eq!(by_name["Alias"], DefKind::Type);
        assert_eq!(by_name["g"], DefKind::Const);
    }

    #[test]
    fn a_case_class_gets_no_synthesized_companion() {
        // Scala mints `object Rec` beside `case class Rec`. Emitting it would
        // make `import p.Rec` resolve to something nobody wrote in
        // preference to the class that is written there.
        let got = defs("package p\ncase class Rec(a: Int)\n");
        assert_eq!(got.iter().filter(|d| d.1 == "Rec").count(), 1, "{got:?}",);
    }

    #[test]
    fn nothing_a_term_declares_is_a_node() {
        let got = defs(
            "package p\n\
             object O {\n\
             \x20 def run(): Unit = {\n\
             \x20   class InBlock\n\
             \x20   object AlsoInBlock\n\
             \x20   def helper(): Int = 1\n\
             \x20 }\n\
             \x20 val anon = new Base { def hidden(): Int = 1\n    class Hidden }\n\
             }\n",
        );
        let names: Vec<&str> = got.iter().map(|d| d.1.as_str()).collect();
        for absent in ["InBlock", "AlsoInBlock", "helper", "hidden", "Hidden"] {
            assert!(!names.contains(&absent), "{absent} in {names:?}");
        }
        // The nameable ones are still there.
        assert!(names.contains(&"run"));
        assert!(names.contains(&"anon"));
    }

    #[test]
    fn a_destructuring_val_declares_nothing_and_an_anonymous_given_neither() {
        let got = defs(
            "package p\nobject O {\n  val (a, b) = (1, 2)\n  val Some(z) = None\n  given Int = 4\n}\n",
        );
        let names: Vec<&str> = got.iter().map(|d| d.1.as_str()).collect();
        assert_eq!(names, ["p", "O"], "{got:?}");
    }

    #[test]
    fn visibility_is_read_off_the_modifiers() {
        let facts = facts(
            "package p\nclass C {\n  private def hidden(): Int = 1\n  protected def kin(): Int = 2\n  def open(): Int = 3\n}\n",
        );
        let find = |name: &str| {
            facts
                .defs
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("{name}"))
                .facets
        };
        assert!(find("hidden").contains(DefFacets::PRIVATE));
        assert!(!find("hidden").contains(DefFacets::EXPORTED));
        assert!(!find("kin").contains(DefFacets::EXPORTED));
        assert!(!find("kin").contains(DefFacets::PRIVATE));
        assert!(find("open").contains(DefFacets::EXPORTED));
    }

    #[test]
    fn a_trait_an_enum_a_record_and_an_abstract_member_carry_their_facets() {
        let facts = facts(
            "package p\ntrait T { def m(): Unit\n  type A }\nenum E { case X }\ncase class R(a: Int)\n",
        );
        let find = |name: &str| {
            facts
                .defs
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("{name}"))
                .facets
        };
        assert!(find("T").contains(DefFacets::INTERFACE));
        assert!(find("E").contains(DefFacets::ENUM));
        assert!(find("R").contains(DefFacets::RECORD));
        assert!(find("m").contains(DefFacets::ABSTRACT));
        assert!(find("A").contains(DefFacets::ABSTRACT));
    }

    #[test]
    fn a_braced_package_declares_its_own_segments() {
        let got = defs("package outer\npackage inner.deep {\n  class C\n}\n");
        let c = got.iter().find(|d| d.1 == "C").expect("C");
        // `package inner.deep { … }` is qualified too: only `inner.deep`
        // opens a scope, exactly as at file scope.
        assert_eq!(
            c.2,
            [
                ".outer".to_string(),
                "..inner".to_string(),
                ".deep".to_string()
            ],
        );
        assert!(got.contains(&(
            DefKind::Module,
            "deep".to_string(),
            vec![".outer".to_string(), "..inner".to_string()]
        )));
        assert!(got.contains(&(
            DefKind::Module,
            "inner".to_string(),
            vec![".outer".to_string()]
        )));
    }

    // -- imports ----------------------------------------------------------

    #[test]
    fn one_reference_per_selector() {
        assert_eq!(raws("package p\nimport a.{B, C}\n"), ["a.B", "a.C"],);
        assert_eq!(forms("package p\nimport a.{B, C}\n"), ["named", "named"]);
    }

    #[test]
    fn a_plain_import_names_its_last_segment() {
        assert_eq!(
            raws("import upickle.core.Visitor\n"),
            ["upickle.core.Visitor"]
        );
        let facts = facts("import upickle.core.Visitor\n");
        assert_eq!(
            facts.refs[0].target.segments,
            ["upickle", "core", "Visitor"],
        );
        assert_eq!(facts.refs[0].kind, RefKind::Import);
        assert_eq!(facts.refs[0].space, DeclSpace::Namespace);
        assert!(!facts.refs[0].locally_bound);
    }

    #[test]
    fn a_wildcard_names_the_container_and_enumerates_nothing() {
        for (source, raw, form) in [
            ("import a.b._\n", "a.b._", "wildcard"),
            ("import a.b.*\n", "a.b.*", "wildcard"),
            ("import a.b.given\n", "a.b.given", "given-wildcard"),
        ] {
            let facts = facts(source);
            assert_eq!(facts.refs.len(), 1, "{source}");
            assert_eq!(facts.refs[0].raw_target, raw);
            assert_eq!(facts.refs[0].target.segments, ["a", "b"], "{source}");
            assert_eq!(facts.header.imports[0].form.name(), form);
        }
    }

    #[test]
    fn a_rename_names_the_original_and_spells_the_site() {
        let facts = facts("import upickle.legacy.{ReadWriter => RW, Reader as R}\n");
        assert_eq!(
            raws("import upickle.legacy.{ReadWriter => RW, Reader as R}\n"),
            [
                "upickle.legacy.ReadWriter => RW",
                "upickle.legacy.Reader as R"
            ],
        );
        assert_eq!(
            facts.refs[0].target.segments,
            ["upickle", "legacy", "ReadWriter"],
        );
        assert_eq!(facts.header.imports[0].form.name(), "renamed");
    }

    #[test]
    fn a_hiding_selector_is_still_a_site_that_names_something() {
        // `import utest.{assert => _, _}` names `utest.assert` in order to
        // hide it. It is a site that names something possibly defined
        // elsewhere, which is the whole of what a reference is.
        assert_eq!(
            raws("import utest.{assert => _, _}\n"),
            ["utest.assert => _", "utest._"],
        );
    }

    #[test]
    fn a_root_qualified_path_keeps_its_marker() {
        let facts = facts("package p\nimport _root_.java.io.File\n");
        assert_eq!(
            facts.refs[0].target.segments,
            ["_root_", "java", "io", "File"]
        );
    }

    #[test]
    fn every_import_reference_is_paired_with_a_clause_by_span() {
        // The pairing is what lets a census read the form a site was written
        // in; an unpaired reference would make one of them invisible.
        let facts = facts(
            "package p\nimport a.{B, C => D, _}\nobject O {\n  import q.r._\n  def f(): Unit = { import s.t.U }\n}\n",
        );
        assert_eq!(facts.refs.len(), 5, "{:?}", raws("x"));
        assert_eq!(facts.header.imports.len(), facts.refs.len());
        let spans: Vec<(u32, u32)> = facts
            .header
            .imports
            .iter()
            .map(|i| (i.span.byte_start, i.span.byte_end))
            .collect();
        for r in &facts.refs {
            assert!(
                spans.contains(&(r.span.byte_start, r.span.byte_end)),
                "unpaired: {}",
                r.raw_target,
            );
        }
    }

    #[test]
    fn an_import_carries_the_container_chain_it_sits_in() {
        let facts = facts(
            "package upickletest\nobject Common {\n  import Recursive._\n  def go(): Unit = { import Late._ }\n}\n",
        );
        let early = facts
            .refs
            .iter()
            .find(|r| r.raw_target == "Recursive._")
            .expect("Recursive._");
        let encloser = early.enclosing.as_ref().expect("an encloser");
        // Marked to the innermost container, so the resolver can read the
        // whole lookup scope back off the path.
        assert_eq!(encloser.path, [".upickletest", ".Common"]);
        assert_eq!(encloser.kind, DefKind::Module);

        let late = facts
            .refs
            .iter()
            .find(|r| r.raw_target == "Late._")
            .expect("Late._");
        let encloser = late.enclosing.as_ref().expect("an encloser");
        assert_eq!(encloser.path, [".upickletest", ".Common", "go"]);
        assert_eq!(encloser.kind, DefKind::Function);
    }

    #[test]
    fn a_file_scope_import_has_no_encloser() {
        let facts = facts("package p\nimport a.B\n");
        assert!(facts.refs[0].enclosing.is_none());
    }

    #[test]
    fn records_come_out_in_source_order() {
        let facts = facts("package p\nimport a.A\nclass C\nobject O\nimport b.B\n");
        let lines: Vec<u32> = facts.refs.iter().map(|r| r.span.line).collect();
        assert_eq!(lines, [2, 5]);
        assert!(
            facts.defs[1..]
                .windows(2)
                .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
            "{:?}",
            facts.defs,
        );
    }

    #[test]
    fn a_broken_file_still_declares_its_package() {
        // tree-sitter is error-tolerant, and a file that does not parse is
        // still a file whose package other files import through.
        let facts = facts("package p.q\nclass ((( \n");
        assert_eq!(facts.defs[0].kind, DefKind::Module);
        assert_eq!(facts.defs[0].name, "q");
    }

    #[test]
    fn the_container_mark_is_the_one_the_grammar_reserves() {
        assert_eq!(CONTAINER_MARK, ".");
        // A definition's *name* is never marked: only the owner chain is,
        // and `def_fqn` reads the definition's own kind for the last step.
        let got = defs("package p\nobject O\n");
        assert!(got.iter().all(|d| !d.1.starts_with(CONTAINER_MARK)));
    }

    #[test]
    fn a_qualified_package_clause_marks_its_intermediate_segments_as_scopeless() {
        // `package ujson.argonaut` puts only `ujson.argonaut`'s members in
        // scope; `ujson`'s are not, which is what stops `import
        // argonaut.Json` from binding to the package one hop up.
        let facts = facts("package ujson.argonaut\nclass C\n");
        assert_eq!(facts.header.package, ["..ujson", ".argonaut"]);
        let c = facts.defs.iter().find(|d| d.name == "C").expect("C");
        assert_eq!(c.owner, ["..ujson", ".argonaut"]);
    }

    #[test]
    fn separate_package_clauses_each_open_a_scope() {
        let facts = facts("package ujson\npackage argonaut\nclass C\n");
        assert_eq!(facts.header.package, [".ujson", ".argonaut"]);
    }
}
