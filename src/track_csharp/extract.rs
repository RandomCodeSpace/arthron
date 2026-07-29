//! One C# file in, records out. Forbidden from linking.
//!
//! # What a tier-2 extractor emits
//!
//! **Definitions, structure, and imports.** Nothing else. The reference kind
//! this module produces is [`RefKind::Import`] and only that: no call site,
//! no type use, no supertype. A tier-2 language that emitted them un-gated
//! would put references in a rate no tier-2 resolver links, which is tier-1
//! coverage claimed without tier-1 work.
//!
//! # The three meanings of `using`
//!
//! C# spells three unrelated things with one keyword, and only position tells
//! them apart:
//!
//! - `using A.B;`, `using static A.B.C;`, `using X = A.B.C;` at compilation
//!   unit or namespace scope — an **import**, and the only one of the three
//!   this module emits. The grammar calls it a `using_directive`.
//! - `using (var x = …) { }` and `using var x = …;` inside a body —
//!   **disposal**. It names a local, and tier 2 emits no expression-level
//!   reference at all.
//! - `extern alias Foo;` — an **assembly alias**, declared in the `.csproj`
//!   and in no source file. Nothing in this graph can be the thing it names,
//!   so no reference is emitted rather than one that could only ever miss.
//!
//! # `#if` is read, never evaluated
//!
//! The extractor reads the file **as written**. tree-sitter keeps a
//! conditional's arms in the tree as ordinary declarations, so a member under
//! `#if FEATURE_SPAN` and another under `#else` are *both* declarations here.
//! That is the only honest answer for this corpus: `Serilog.csproj` declares
//! seven target frameworks on Windows and five elsewhere, and each defines a
//! different set of `FEATURE_*` symbols, so there is no single build to
//! prefer. The union over configurations is the superset, and it is the same
//! judgement [`crate::track_rust`] makes for `#[cfg]`.
//!
//! **What that costs, measured rather than assumed:** a `#if` whose arms are
//! not whole declarations — the corpus splits a method's *signature* from its
//! body across `#if`/`#else`/`#endif` — is more than tree-sitter-c-sharp's
//! error recovery can carry. The measured threshold is **three**: one or two
//! such splits in a type recover cleanly and both arms declare, and the third
//! collapses the enclosing type declaration into an `ERROR` node. The members
//! survive the recovery and their owner does not, so in such a file neither
//! the type nor any member of it is extracted — C# has no member outside a
//! type, so an empty owner chain means the tree is an artefact and no owner
//! is guessed back out of it.
//!
//! Two of the corpus's 193 files are in that shape: `src/Serilog/ILogger.cs`
//! and `src/Serilog/Capturing/PropertyBinder.cs`. Their namespaces and their
//! `using` directives still parse and are still emitted.
//!
//! # Known non-claims, recorded rather than left to be rediscovered
//!
//! - **An anonymous type and a lambda declare nothing nameable**, so nothing
//!   is emitted for them. The same judgement Java and PHP make for an
//!   anonymous class body.
//! - **A class primary constructor declares no property.** C# promotes a
//!   positional parameter to a property for a `record` and not for a `class`,
//!   so only the record form mints one.
//! - **`partial` is not tracked as a facet.** Two halves of a partial type
//!   share a name, an owner and a kind, which is exactly what makes them one
//!   entity to [`crate::track_csharp::resolve::CsResolver::mergeable`];
//!   nothing else here reads the keyword.
//! - **An alias to a type *expression*** — `using X = int[];`,
//!   `using X = (int, string);` — names no declared entity, so it produces no
//!   reference. An alias to a *generic* name does: `using X = List<int>;`
//!   names ``List`1`` — the open type, spelled with the arity .NET files the
//!   declaration under, so an in-repository `class Box<T>` is reachable
//!   through one. Its type *arguments* are type uses tier 2 does not resolve,
//!   and only their number is read. The corpus contains neither shape, so the
//!   fixtures in `tests/csharp_extract.rs` and `tests/csharp_resolve.rs` are
//!   what hold this.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, Params, RefKind, RefTarget, Reference,
    Span, TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_csharp::lang::{CsLang, arity_name};

/// The embedded C# extraction rules.
const CSHARP_RULES: &str = include_str!("../rules/csharp.yml");

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules::compile(CSHARP_RULES).expect("embedded csharp.yml compiles"))
}

/// Which of C#'s tables a `using` directive reads.
///
/// The whole of C#'s import model, and the distinction the resolver branches
/// on: a plain `using` must name a namespace, `using static` must name a
/// type, and an alias may name either — C# lets `using X = A.B;` alias a
/// namespace as readily as a type, and nothing at the site says which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportForm {
    /// `using A.B;` — names a namespace, and only a namespace.
    Namespace(Vec<String>),
    /// `using static A.B.C;` — names a type, and only a type.
    Static(Vec<String>),
    /// `using X = A.B.C;` — names a type *or* a namespace.
    Alias {
        /// The name bound in this file.
        alias: String,
        /// The name it is bound to, segment by segment.
        target: Vec<String>,
    },
}

/// One `using` directive: what it spells, how far it reaches, and where it
/// sits.
///
/// Every `ImportSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is how the
/// resolver pairs the two without the core learning what a `using` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    /// Which table the directive reads, and what it names.
    pub form: ImportForm,
    /// `global using`: the directive reaches every file in its *project*
    /// rather than its own file.
    ///
    /// A fact about scope, not about the target, so it changes nothing this
    /// resolver decides: what a `global using` *binds* matters only to the
    /// expression-level references tier 2 does not emit. Carried because it
    /// is the reason 169 of the corpus's 193 files name no import at all,
    /// and a reader of the tally needs to be able to see that.
    pub global: bool,
    /// Where the directive sits. The whole clause, so the key is unique.
    pub span: Span,
}

/// What one C# file states about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CsHeader {
    /// The file's repository-relative path.
    pub rel_path: String,
    /// Every namespace this file declares, in source order.
    ///
    /// A `Vec` and not an `Option`, because the file-to-namespace mapping is
    /// not a function: braced `namespace X { … }` blocks let one file
    /// contribute to several. A file that declares none carries one entry,
    /// the empty string — the global namespace is a container with no name,
    /// not the absence of a container.
    pub namespaces: Vec<String>,
    /// Every `using` directive, in source order.
    pub imports: Vec<ImportSpec>,
}

/// The namespace blocks a file declares, as byte ranges.
///
/// Two shapes, and C# allows only one of them per file: a braced
/// `namespace N { … }` owns its own byte range, and a file-scoped
/// `namespace N;` owns everything from its start to the end of the file.
struct Blocks {
    spans: Vec<(u32, u32, String)>,
}

impl Blocks {
    fn build(decls: &[SgNode], source_len: u32) -> Blocks {
        let mut spans = Vec::with_capacity(decls.len());
        for node in decls {
            let span = span_of(node);
            let end = if node.kind() == "namespace_declaration" {
                span.byte_end
            } else {
                source_len
            };
            spans.push((span.byte_start, end, namespace_name(node)));
        }
        Blocks { spans }
    }

    /// The namespace a byte offset sits in: the **innermost** declaration
    /// whose block contains it, which is what C# says when one braced
    /// declaration nests inside another. Chosen by block width rather than by
    /// position in the match list, so the answer cannot depend on the order
    /// the rules happened to report two declarations in.
    ///
    /// The global namespace — `""` — is the answer for a file that declares
    /// none, for the code above a file-scoped declaration, and for the code
    /// beside a braced one.
    fn at(&self, byte: u32) -> &str {
        self.spans
            .iter()
            .filter(|(start, end, _)| *start <= byte && byte < *end)
            .min_by_key(|(start, end, _)| end - start)
            .map_or("", |(_, _, name)| name.as_str())
    }
}

/// Every namespace declaration kind. C# writes a namespace two ways and
/// allows only one of the two per file, but a braced declaration may nest
/// inside another braced one.
const NAMESPACE_KINDS: [&str; 2] = ["namespace_declaration", "file_scoped_namespace_declaration"];

/// The full name a namespace declaration states, composed with every
/// namespace declaration enclosing it.
///
/// `namespace Alpha { namespace Beta { … } }` is C#'s other spelling of
/// `namespace Alpha.Beta`, and the two have to reach one identity. Reading
/// only the `name` field would declare a namespace `Beta` this repository
/// does not have, file `Widget` under it as `Beta#Widget`, and leave
/// `using Alpha.Beta;` — a namespace this repository *does* declare — looking
/// like somebody else's assembly.
///
/// That last part is why this is not a cosmetic difference:
/// [`crate::track_csharp::resolve`] answers a namespace it cannot find with
/// `External`, which sits outside **both** terms of the resolution rate, so a
/// resolver bug caused by a name this module got wrong would leave the rate
/// untouched rather than counting against it.
/// [`crate::track_csharp::lang::implied_namespaces`] closes the same hole for
/// the dotted spelling; this closes it for the nested one.
///
/// A step the grammar recovered without a name contributes nothing rather
/// than an empty segment, so an artefact above a named block cannot mint
/// `.Beta`. `""` — the global namespace — is what a chain with no readable
/// name at all states.
fn namespace_name(node: &SgNode) -> String {
    let mut parts: Vec<String> = node
        .ancestors()
        .filter(|a| NAMESPACE_KINDS.contains(&&*a.kind()))
        .filter_map(|a| a.field("name"))
        .map(|name| name.text().to_string())
        .collect();
    parts.reverse();
    parts.extend(node.field("name").map(|name| name.text().to_string()));
    parts.retain(|part| !part.is_empty());
    parts.join(".")
}

/// The type declarations enclosing a node, outermost first, each name
/// carrying its own arity.
///
/// `None` when a type declaration on the way up has no name the grammar could
/// read — a recovery artefact, which nothing can name and so nothing may be
/// filed under.
fn enclosing_types(node: &SgNode) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for a in node.ancestors() {
        if !TYPE_KINDS.contains(&&*a.kind()) {
            continue;
        }
        let name = a.field("name")?;
        out.push(arity_name(&name.text(), type_arity(&a)));
    }
    out.reverse();
    Some(out)
}

/// Every type declaration kind. A delegate is one too — it declares a named
/// type — but it can hold no member, so it never appears in an owner chain.
const TYPE_KINDS: [&str; 5] = [
    "class_declaration",
    "struct_declaration",
    "interface_declaration",
    "record_declaration",
    "enum_declaration",
];

/// How many type parameters a declaration writes.
fn type_arity(node: &SgNode) -> usize {
    node.children()
        .find(|c| c.kind() == "type_parameter_list")
        .map(|list| {
            list.children()
                .filter(|c| c.kind() == "type_parameter")
                .count()
        })
        .unwrap_or(0)
}

/// How many type arguments a `generic_name` writes — the arity of the type it
/// names. `Box<int>` writes one, `Dictionary<string, int>` two.
///
/// The list's children are counted with its own punctuation removed, rather
/// than filtered for one node kind: a type argument is spelled with whatever
/// node its type needs — `identifier`, `qualified_name`, `predefined_type`,
/// `nullable_type`, `array_type`, a nested `generic_name` — and there is no
/// single kind to look for. An unbound `List<>` writes none and is arity 0
/// here; it is legal only inside `typeof`, which tier 2 does not read.
fn type_argument_count(node: &SgNode) -> usize {
    node.children()
        .find(|c| c.kind() == "type_argument_list")
        .map(|list| {
            list.children()
                .filter(|c| !matches!(&*c.kind(), "<" | ">" | "," | "comment"))
                .count()
        })
        .unwrap_or(0)
}

/// Whether a declaration carries a given modifier keyword.
fn has_modifier(node: &SgNode, word: &str) -> bool {
    node.children()
        .any(|c| c.kind() == "modifier" && c.text().trim() == word)
}

/// The interface an explicit implementation names, with its trailing dot
/// removed: `IEnumerable<T>.GetEnumerator()` declares
/// `IEnumerable<T>.GetEnumerator`.
///
/// A member C# does *not* merge with the ordinary member of the same name: a
/// type may declare `GetEnumerator()`, `IEnumerable<T>.GetEnumerator()` and
/// `IEnumerable.GetEnumerator()` at once, and the corpus does. The qualifier
/// is the only thing that tells the three apart, so it belongs in the name —
/// which is where .NET metadata puts it too.
fn declared_name(node: &SgNode, name: &str) -> String {
    match node
        .children()
        .find(|c| c.kind() == "explicit_interface_specifier")
    {
        Some(spec) => format!("{}.{name}", spec.text().trim_end_matches('.').trim(),),
        None => name.to_string(),
    }
}

/// The facets every declaration form shares.
///
/// `PRIVATE` is set only where the keyword is *written*. A C# member with no
/// access modifier is private too, and this deliberately does not infer that:
/// nothing at tier 2 reads the bit — it exists for the supertype closure a
/// tier-1 language builds — and a facet inferred where no keyword stands is a
/// claim this track has no use for.
fn facets_of(node: &SgNode) -> DefFacets {
    let mut facets = DefFacets::default();
    for (word, bit) in [
        ("static", DefFacets::STATIC),
        ("abstract", DefFacets::ABSTRACT),
        ("public", DefFacets::EXPORTED),
        ("private", DefFacets::PRIVATE),
    ] {
        if has_modifier(node, word) {
            facets = facets.union(bit);
        }
    }
    facets
}

/// The parameter shape of a callable, read off a parameter list.
///
/// The list is split on its commas rather than filtered for `parameter`
/// nodes, because tree-sitter-c-sharp does not wrap a **`params`** parameter
/// in one: it flattens the keyword, the type and the name straight into the
/// list, where every other parameter — `ref`, `out`, `scoped`, attributed,
/// defaulted — arrives as a proper `parameter` with `name` and `type` fields.
/// Reading only the wrapped ones loses the flattened parameter entirely, and
/// the corpus declares `Push(params ReadOnlySpan<T>)`,
/// `Push(params T[])` and `Push(params IEnumerable<T>)` side by side: three
/// methods that would hash to one node, and one node's edges standing in for
/// three declarations' worth of sites.
fn params_of(list: Option<SgNode>) -> Params {
    let Some(list) = list else {
        return Params {
            count: 0,
            varargs: false,
            types: Vec::new(),
        };
    };
    let mut groups: Vec<Vec<SgNode>> = vec![Vec::new()];
    for child in list.children() {
        match &*child.kind() {
            "(" | ")" | "[" | "]" => {}
            "," => groups.push(Vec::new()),
            _ => groups.last_mut().expect("one group always").push(child),
        }
    }
    let mut types: Vec<String> = Vec::new();
    let mut varargs = false;
    for group in groups.iter().filter(|g| !g.is_empty()) {
        if let [only] = &group[..]
            && only.kind() == "parameter"
        {
            varargs = has_modifier(only, "params");
            types.push(type_spelling(only.field("type")));
            continue;
        }
        // The flattened `params` form: `params <type> <name>`.
        varargs = varargs || group.iter().any(|c| c.kind() == "params");
        let name_at = group.iter().rposition(|c| c.kind() == "identifier");
        let declared = name_at
            .and_then(|at| at.checked_sub(1))
            .map(|at| group[at].clone());
        types.push(type_spelling(declared));
    }
    Params {
        count: types.len() as u32,
        varargs,
        types,
    }
}

/// A type node's source spelling, whitespace collapsed so that a type written
/// across two lines and the same type written on one hash alike.
fn type_spelling(node: Option<SgNode>) -> String {
    node.map(|t| t.text().split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// The type parameters of a generic type, as a [`Params`].
///
/// C# discriminates *types* by arity the way it discriminates methods by
/// signature — `Foo<T>` and `Foo<T, U>` are two types — so a type's parameter
/// shape is exactly what [`Params`] is documented for.
fn type_params(node: &SgNode) -> Option<Params> {
    let list = node
        .children()
        .find(|c| c.kind() == "type_parameter_list")?;
    let names: Vec<String> = list
        .children()
        .filter(|c| c.kind() == "type_parameter")
        .map(|c| {
            c.field("name")
                .map(|n| n.text().to_string())
                .unwrap_or_default()
        })
        .collect();
    Some(Params {
        count: names.len() as u32,
        varargs: false,
        types: names,
    })
}

/// One definition, with the fields every C# declaration shares.
fn cs_def(
    kind: DefKind,
    name: String,
    owner: Vec<String>,
    space: DeclSpace,
    facets: DefFacets,
    params: Option<Params>,
    span: Span,
) -> Definition {
    Definition {
        kind,
        name,
        owner,
        space,
        facets,
        params,
        span,
    }
}

/// The C# extractor. One path and one source string; nothing to link with.
pub struct CsExtractor;

impl Extractor<CsLang> for CsExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<CsLang> {
        extract(rel_path, source)
    }
}

/// Extract all facts from one C# source file.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<CsLang> {
    let tree = SourceTree::parse_csharp(source);
    let matches = tree.matches(rules());
    let source_len = source.len().min(u32::MAX as usize) as u32;
    let namespace_decls: Vec<SgNode> = matches
        .iter()
        .filter(|(_, n)| {
            matches!(
                &*n.kind(),
                "namespace_declaration" | "file_scoped_namespace_declaration"
            )
        })
        .map(|(_, n)| n.clone())
        .collect();
    let blocks = Blocks::build(&namespace_decls, source_len);

    let mut header = CsHeader {
        rel_path: rel_path.to_string(),
        namespaces: Vec::new(),
        imports: Vec::new(),
    };
    let mut defs: Vec<Definition> = Vec::new();
    let mut refs: Vec<Reference> = Vec::new();

    // Every file has the global namespace above it, so every file emits it —
    // not only the ones that declare no other. C# has no syntax for declaring
    // the global namespace at all: it is the scope a compilation unit begins
    // in, and a file writing `namespace N;` puts that very declaration in it.
    // (This is where C# parts company with `track_php`, whose global
    // namespace *is* writable as `namespace { … }` and so is a thing a file
    // either opts into or does not.)
    //
    // Minting it unconditionally is what keeps two claims true. A definition
    // at file scope beside a braced `namespace N { … }` has an owner
    // container that exists — the container is what a tier-2 track delivers,
    // and one that names a node nobody declared is a dangling frame. And a
    // one-segment type-shaped miss — `using static Math;` — probes the global
    // namespace, hits, and is classified `NoMatchingDefinition`, the answer
    // that counts *against* the rate; had the container's existence depended
    // on some unrelated file in the repository declaring no namespace, one
    // source line would have been `External` in one repository and a miss in
    // another (see the `resolve` module docs, rule 5).
    //
    // The three `GlobalUsings.cs` files, which carry 65 of the corpus's 89
    // imports, declare no namespace at all and so were already reaching it by
    // the narrower rule; what changes is every other file.
    //
    // `header.namespaces` still lists only what the file *declares*, and so
    // carries the empty string only for a file that declares nothing else:
    // the global namespace is not declared, and a header that claimed
    // otherwise would be stating a keyword that is not in the file.
    if namespace_decls.is_empty() {
        header.namespaces.push(String::new());
    }
    defs.push(cs_def(
        DefKind::Module,
        String::new(),
        Vec::new(),
        DeclSpace::Namespace,
        DefFacets::default(),
        None,
        Span {
            byte_start: 0,
            byte_end: source_len,
            line: 1,
        },
    ));

    for (_, node) in &matches {
        let span = span_of(node);
        let namespace = blocks.at(span.byte_start).to_string();
        match &*node.kind() {
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                declare_namespace(&mut header, &mut defs, node, span);
            }
            "using_directive" => {
                if let Some(spec) = import(node, span) {
                    refs.push(reference(node, &spec, namespace));
                    header.imports.push(spec);
                }
            }
            "delegate_declaration" => {
                // A delegate declares a named type and holds no member, so it
                // never appears in an owner chain — but it may itself be
                // declared inside one.
                type_declaration(&mut defs, node, &namespace, span, DefFacets::default());
            }
            kind if TYPE_KINDS.contains(&kind) => {
                let mut facets = facets_of(node);
                if kind == "interface_declaration" {
                    facets = facets.union(DefFacets::INTERFACE);
                }
                if kind == "enum_declaration" {
                    facets = facets.union(DefFacets::ENUM);
                }
                if kind == "record_declaration" {
                    facets = facets.union(DefFacets::RECORD);
                }
                type_declaration(&mut defs, node, &namespace, span, facets);
                if kind == "record_declaration" {
                    // A positional record parameter *is* a public property.
                    // A class primary constructor's is not, so only this
                    // branch mints them.
                    positional_properties(&mut defs, node, &namespace);
                }
            }
            other => member(&mut defs, node, other, &namespace, span),
        }
    }

    FileFacts { header, defs, refs }
}

/// A namespace declaration, plus every namespace it implies.
fn declare_namespace(header: &mut CsHeader, defs: &mut Vec<Definition>, node: &SgNode, span: Span) {
    let name = namespace_name(node);
    header.namespaces.push(name.clone());
    let module = |name: String, facets: DefFacets| {
        cs_def(
            DefKind::Module,
            name,
            Vec::new(),
            DeclSpace::Namespace,
            facets,
            None,
            span,
        )
    };
    defs.push(module(name.clone(), DefFacets::default()));
    for implied in crate::track_csharp::lang::implied_namespaces(&name) {
        // Synthesized: the file wrote `namespace A.B.C;` and C# read three
        // declarations out of it. `using A.B;` names one of them.
        defs.push(module(implied, DefFacets::SYNTHETIC));
    }
}

/// A class, struct, interface, record, enum or delegate declaration.
fn type_declaration(
    defs: &mut Vec<Definition>,
    node: &SgNode,
    namespace: &str,
    span: Span,
    facets: DefFacets,
) {
    let Some(name) = node.field("name") else {
        return; // a recovery artefact: nothing can name it
    };
    let Some(outer) = enclosing_types(node) else {
        return;
    };
    let mut owner = vec![namespace.to_string()];
    owner.extend(outer);
    defs.push(cs_def(
        DefKind::Type,
        name.text().to_string(),
        owner,
        DeclSpace::Type,
        facets,
        type_params(node),
        span,
    ));
}

/// The properties a positional record declares, one per parameter.
fn positional_properties(defs: &mut Vec<Definition>, node: &SgNode, namespace: &str) {
    let Some(name) = node.field("name") else {
        return;
    };
    let Some(outer) = enclosing_types(node) else {
        return;
    };
    // A record's positional list is a plain child: unlike a method's, the
    // grammar hangs no `parameters` field on it.
    let Some(list) = node.children().find(|c| c.kind() == "parameter_list") else {
        return;
    };
    let mut owner = vec![namespace.to_string()];
    owner.extend(outer);
    owner.push(arity_name(&name.text(), type_arity(node)));
    for parameter in list.children().filter(|c| c.kind() == "parameter") {
        let Some(pname) = parameter.field("name") else {
            continue;
        };
        defs.push(cs_def(
            DefKind::Property,
            pname.text().to_string(),
            owner.clone(),
            DeclSpace::Value,
            DefFacets::SYNTHETIC.union(DefFacets::EXPORTED),
            None,
            span_of(&parameter),
        ));
    }
}

/// A member declaration of any form.
///
/// A member whose enclosing type the parse lost declares nothing here: C# has
/// no member outside a type, so an empty owner chain means the tree is a
/// recovery artefact and the owner would have to be guessed.
fn member(defs: &mut Vec<Definition>, node: &SgNode, kind: &str, namespace: &str, span: Span) {
    let Some(outer) = enclosing_types(node) else {
        return;
    };
    if outer.is_empty() {
        return;
    }
    let mut owner = vec![namespace.to_string()];
    owner.extend(outer);
    let facets = facets_of(node);
    let named = |name: String, kind: DefKind, facets: DefFacets, params: Option<Params>, span| {
        cs_def(
            kind,
            name,
            owner.clone(),
            DeclSpace::Value,
            facets,
            params,
            span,
        )
    };
    match kind {
        "method_declaration" => {
            let Some(name) = node.field("name") else {
                return;
            };
            defs.push(named(
                declared_name(node, &name.text()),
                DefKind::Method,
                facets,
                Some(params_of(node.field("parameters"))),
                span,
            ));
        }
        "constructor_declaration" => {
            let Some(name) = node.field("name") else {
                return;
            };
            defs.push(named(
                name.text().to_string(),
                DefKind::Constructor,
                facets,
                Some(params_of(node.field("parameters"))),
                span,
            ));
        }
        "destructor_declaration" => {
            let Some(name) = node.field("name") else {
                return;
            };
            // The finalizer, named the way the source names it. C# calls it
            // `Finalize` in metadata; nothing here reads metadata, and the
            // source spelling is the one a reader of a report will recognise.
            defs.push(named(
                format!("~{}", name.text()),
                DefKind::Method,
                facets,
                Some(params_of(node.field("parameters"))),
                span,
            ));
        }
        "operator_declaration" | "conversion_operator_declaration" => {
            // An operator is a method whose name is a keyword and a token.
            // The source spelling is used rather than the metadata one
            // (`op_Addition`, `op_Implicit`): translating between the two is
            // a table, and a table nothing in the corpus exercises is a guess
            // written down.
            let name = operator_name(node);
            if name.is_empty() {
                return;
            }
            defs.push(named(
                name,
                DefKind::Method,
                facets,
                Some(params_of(node.field("parameters"))),
                span,
            ));
        }
        "property_declaration" => {
            let Some(name) = node.field("name") else {
                return;
            };
            defs.push(named(
                declared_name(node, &name.text()),
                DefKind::Property,
                facets,
                None,
                span,
            ));
        }
        "indexer_declaration" => {
            // An indexer is a property with no name of its own; `this[]` is
            // what the source calls it, and `[` cannot appear in a C#
            // identifier, so no declared name can forge it.
            defs.push(named(
                declared_name(node, "this[]"),
                DefKind::Property,
                facets,
                Some(params_of(node.field("parameters"))),
                span,
            ));
        }
        "event_declaration" => {
            let Some(name) = node.field("name") else {
                return;
            };
            // An event is the add/remove pair a `+=` names, never the
            // backing field the field-like form also generates — which is
            // why both forms are filed the way a property is.
            defs.push(named(
                declared_name(node, &name.text()),
                DefKind::Property,
                facets,
                None,
                span,
            ));
        }
        "event_field_declaration" | "field_declaration" => {
            let event = kind == "event_field_declaration";
            let constant = has_modifier(node, "const");
            let Some(declaration) = node.children().find(|c| c.kind() == "variable_declaration")
            else {
                return;
            };
            for declarator in declaration
                .children()
                .filter(|c| c.kind() == "variable_declarator")
            {
                let Some(name) = declarator.children().find(|c| c.kind() == "identifier") else {
                    continue;
                };
                let kind = match (event, constant) {
                    (true, _) => DefKind::Property,
                    (false, true) => DefKind::Const,
                    (false, false) => DefKind::Field,
                };
                defs.push(named(
                    name.text().to_string(),
                    kind,
                    facets,
                    None,
                    span_of(&declarator),
                ));
            }
        }
        "enum_member_declaration" => {
            let Some(name) = node.field("name") else {
                return;
            };
            // An enum member is a constant of its enum, in the same table a
            // `const` field lives in — which is what C# makes it.
            defs.push(named(
                name.text().to_string(),
                DefKind::Const,
                facets,
                None,
                span,
            ));
        }
        _ => {}
    }
}

/// The name an operator declaration writes: `operator +`, `implicit operator
/// string`.
fn operator_name(node: &SgNode) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut seen_operator = false;
    for child in node.children() {
        match &*child.kind() {
            "modifier" => {}
            "implicit" | "explicit" => parts.push(child.text().to_string()),
            "operator" => {
                parts.push("operator".to_string());
                seen_operator = true;
            }
            "parameter_list" | "block" | "arrow_expression_clause" | ";" => break,
            // The operator token itself, and C# 11's `checked` before it.
            // Everything ahead of the keyword is the return type, which is
            // not part of the name.
            _ if seen_operator => parts.push(child.text().to_string()),
            _ => {}
        }
    }
    if !seen_operator {
        return String::new();
    }
    parts.join(" ")
}

/// One `using` directive, or `None` when it names no declared entity.
fn import(node: &SgNode, span: Span) -> Option<ImportSpec> {
    let global = node.children().any(|c| c.kind() == "global");
    let is_static = node.children().any(|c| c.kind() == "static");
    let alias = node.field("name").map(|n| n.text().to_string());
    // The target is the last name-shaped child, and for an alias it is the
    // last one *after the `=`* — the bound name is a name-shaped child too,
    // and `using File = File;` would otherwise have the two indistinguishable.
    let mut after_equals = false;
    let target = node
        .children()
        .filter(|c| {
            if c.kind() == "=" {
                after_equals = true;
            }
            after_equals || alias.is_none()
        })
        .filter(|c| matches!(&*c.kind(), "qualified_name" | "identifier" | "generic_name"))
        .last()?;
    let segments = name_segments(&target)?;
    let form = match (is_static, alias) {
        (true, _) => ImportForm::Static(segments),
        (false, Some(alias)) => ImportForm::Alias {
            alias,
            target: segments,
        },
        (false, None) => ImportForm::Namespace(segments),
    };
    Some(ImportSpec { form, global, span })
}

/// The segments of a dotted name, or `None` when the node is not one.
///
/// A `generic_name` contributes its own identifier **carrying its arity** and
/// nothing else: `List<int>` is a segment ``List`1``. Its type arguments are
/// type uses and tier 2 resolves no type, but how *many* of them stand there
/// is not a type — it is part of the name .NET files the declaration under,
/// and part of the name [`crate::track_csharp::lang::type_fqn`] builds from
/// `class List<T>`. Dropping it would spell a key ``List`` that no declared
/// generic type can ever match, so `using X = Ns.Box<int>;` could not reach
/// an in-repository ``Ns#Box`1`` however plainly it names it.
fn name_segments(node: &SgNode) -> Option<Vec<String>> {
    match &*node.kind() {
        "identifier" => Some(vec![node.text().to_string()]),
        "generic_name" => node
            .children()
            .find(|c| c.kind() == "identifier")
            .map(|c| vec![arity_name(&c.text(), type_argument_count(node))]),
        "qualified_name" => {
            let mut out = Vec::new();
            for child in node.children() {
                if child.kind() == "." {
                    continue;
                }
                out.extend(name_segments(&child)?);
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
}

/// The [`RefKind::Import`] reference one directive states.
fn reference(node: &SgNode, spec: &ImportSpec, namespace: String) -> Reference {
    let (space, segments, written) = match &spec.form {
        ImportForm::Namespace(segments) => (DeclSpace::Namespace, segments, segments.join(".")),
        ImportForm::Static(segments) => (DeclSpace::Type, segments, segments.join(".")),
        ImportForm::Alias { alias, target } => (
            // A name the site does not say which table to read in. `Type` is
            // the table the resolver tries first and never the last word:
            // `using X = A.B;` may alias a namespace, and the resolver tries
            // that too rather than trusting this field.
            DeclSpace::Type,
            target,
            format!("{alias} = {}", target.join(".")),
        ),
    };
    let keyword = match (spec.global, matches!(spec.form, ImportForm::Static(_))) {
        (true, true) => "global using static ",
        (true, false) => "global using ",
        (false, true) => "using static ",
        (false, false) => "using ",
    };
    Reference {
        kind: RefKind::Import,
        space,
        raw_target: format!("{keyword}{written}"),
        target: RefTarget {
            root: TargetRoot::Name,
            segments: segments.clone(),
        },
        // Structurally false: a `using` names an absolute name, and no block
        // binds one. `LocalBinding` does not apply at tier 2 — there is no
        // expression-level reference to be bound.
        locally_bound: false,
        argc: None,
        arg_types: None,
        // A directive inside a braced `namespace N { using …; }` belongs to
        // that namespace. One at compilation-unit scope belongs to no
        // definition, and the driver sources it at the file's own container.
        enclosing: node
            .ancestors()
            .any(|a| a.kind() == "namespace_declaration")
            .then(|| Encloser {
                path: vec![namespace],
                kind: DefKind::Module,
            }),
        span: spec.span,
    }
}
