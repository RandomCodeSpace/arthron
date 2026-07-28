//! C++ extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/cpp.yml`) select nodes by kind; this
//! module interprets their fields.
//!
//! # What a tier-2 extractor emits, and what it must not
//!
//! Definitions and structure, plus **include and module references and
//! nothing else**. C++'s gate is an import-resolution rate, so a call site or
//! a type use emitted here would enter a denominator nothing in this track
//! can resolve — tier-1 coverage claimed without tier-1 measurement. A base
//! clause is therefore read as part of the derived class's structure and
//! produces no [`RefKind::Inherit`] reference.
//!
//! # The preprocessor is not evaluated
//!
//! Every `#include` in the file is a reference, including the ones under a
//! `#if` this build cannot decide. `src/fmt.cc` alone puts 34 of them inside
//! `#ifndef FMT_IMPORT_STD`. Evaluating the branches would need a macro
//! environment that only a real compilation has, and picking one branch would
//! make the measurement depend on a platform nobody named. The honest reading
//! is the whole file, and it is stated here rather than left to be inferred
//! from a count.
//!
//! Holding that guarantee costs one pass over the bytes before the parse.
//! `__has_include(<version>)` is a preprocessor operator the pinned grammar
//! has no rule for, and when it is not the whole condition the misparse does
//! not stop at the directive — it swallows the rest of the file, and every
//! `#include` after it stops existing. See [`defuse_header_names`]: the
//! condition's header name is replaced by an equal-length filler, no branch
//! is decided, and no span moves.
//!
//! # C++20 modules, against a grammar that has none
//!
//! The pinned tree-sitter C++ grammar does not know module declarations.
//! `export module fmt;` comes back as a `declaration` whose third child is an
//! `ERROR` node, and `import fmt;` comes back as a `declaration` that is
//! shaped exactly like a variable `fmt` of a type named `import`. So the
//! module directives are read off the token sequence the misparse leaves,
//! narrowly and by fixture, rather than from a grammar rule that does not
//! exist. `module;` — the global module fragment — parses as an
//! `expression_statement` instead and is deliberately not read: it opens the
//! global module, it does not name one.
//!
//! # Recorded under-counts
//!
//! Each is a known shortfall, written down rather than left to be
//! rediscovered, and none may be closed by widening a bucket:
//!
//! - **Preprocessor macros are not definitions.** `#define` declares a name
//!   in the preprocessor's space, not in C++'s: it has no scope, no linkage
//!   and no owner, two translation units that both `#define FMT_OS_H_` do not
//!   declare one entity, and nothing at tier 2 names one. This is the same
//!   line the Rust track draws at struct fields.
//! - **Data members are not definitions**, for the same reason: nothing at
//!   tier 2 names one. Member *functions* are structure and are emitted.
//! - **An unnamed namespace names nothing.** Its contents have internal
//!   linkage and a distinct identity per translation unit, and this build has
//!   no per-unit entity space, so a declaration inside one is not filed at
//!   all rather than filed under an identity two units would share.
//! - **A declaration inside a function body** — a local class, a local
//!   `static` — is not filed either. Its owner is a block, and a block is not
//!   a node.
//! - **An out-of-line member definition understates its kind.** `void
//!   buffer::append() {}` is filed under owner `…::buffer` as a
//!   `DefKind::Function`, because one file cannot say whether `buffer` is a
//!   class or a namespace. The identity is right either way; only the kind is
//!   weaker than the truth.
//! - **An overload set collapses.** Two functions of one name under one owner
//!   are one identity here; C++ discriminates them by parameter types, which
//!   is type resolution and is exactly what tier 2 does not claim.
//! - **A conversion function is not filed.** `A::operator bool()` states no
//!   return type and its declarator is an `operator_cast` node whose name is
//!   the target type; that is a spelling this identity space has no rule for,
//!   and inventing one for three sites would be a guess.
//! - **A module partition or header unit** — `import :part;`,
//!   `import <vector>;` — contributes no reference. The corpus contains none,
//!   and a shape nothing measured is a shape this build does not guess at.

use std::borrow::Cow;
use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_cpp::lang::CppLang;

/// The embedded C++ extraction rules.
const CPP_RULES: &str = include_str!("../rules/cpp.yml");

/// How an import clause spells what it names.
///
/// The distinction is the whole of C++'s import model: a quoted `#include`
/// starts at the including file's own directory, an angled one starts at the
/// include roots, a C++20 `import` names no path at all, and a specifier that
/// is not a literal names nothing this build can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncludeForm {
    /// `#include "…"` — the including file's directory first, then the roots.
    Quoted(String),
    /// `#include <…>` — the include roots only.
    Angle(String),
    /// A C++20 `import <name>;`, or a `module <name>;` implementation-unit
    /// declaration. Names a module, never a path.
    Module(String),
    /// `#include SOME_MACRO` — the specifier is not a literal. Never guessed.
    Computed,
}

/// One import clause: what it spells plus where it sits.
///
/// Every `IncludeSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is how the
/// resolver pairs the two without the core learning what an `#include` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeSpec {
    /// What the clause spells.
    pub form: IncludeForm,
    /// Where the clause sits. The whole directive, so the key is unique.
    pub span: Span,
}

/// Per-file C++ facts only the C++ resolver reads.
///
/// `rel_path` is here because a quoted `#include` is resolved against *where
/// the file is*, and the core must not be the layer that turns a path into an
/// include candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CppHeader {
    /// Repository-relative, `/`-separated path of the file.
    pub rel_path: String,
    /// Every import clause, in source order.
    pub includes: Vec<IncludeSpec>,
}

/// One lexical frame between a node and the top of its file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    /// The frame's name, as written.
    name: String,
    /// A class, struct, union or enum, rather than a namespace.
    is_type: bool,
}

/// Node kinds that open a type scope.
const TYPE_SCOPES: [&str; 4] = [
    "class_specifier",
    "struct_specifier",
    "union_specifier",
    "enum_specifier",
];

/// The lexical frames enclosing a node, outermost first.
///
/// `None` when a frame is crossed that no lexical path can name: an unnamed
/// namespace or an anonymous type, whose members have no identity two
/// translation units could agree on, and a function body, whose declarations
/// are owned by a block. Answering `None` is what keeps a node from being
/// invented for a declaration whose owner this file does not state.
fn frames(node: &SgNode) -> Option<Vec<Frame>> {
    let mut out = Vec::new();
    for a in node.ancestors() {
        let kind = a.kind().to_string();
        if kind == "namespace_definition" {
            // An unnamed namespace has no `name` field at all.
            let name = a.field("name")?;
            // Innermost first, because the whole list is reversed at the end:
            // `namespace fmt::inline v11` is two frames of one ancestor and
            // their order within it must survive that reversal.
            for segment in namespace_segments(&name).into_iter().rev() {
                out.push(Frame {
                    name: segment,
                    is_type: false,
                });
            }
        } else if TYPE_SCOPES.contains(&kind.as_str()) {
            let name = a.field("name")?;
            out.push(Frame {
                name: name.text().to_string(),
                is_type: true,
            });
        } else if matches!(
            kind.as_str(),
            "function_definition" | "lambda_expression" | "compound_statement"
        ) {
            return None;
        }
    }
    out.reverse();
    Some(out)
}

/// The segments a namespace name node spells: `fmt` → `["fmt"]`,
/// `fmt::inline v11` → `["fmt", "v11"]`.
///
/// An inline namespace is transparent to lookup, and its members are named
/// through the enclosing namespace as often as through it. Both spellings
/// cannot be one identity, so the one written is kept and the transparency is
/// not modelled — a limit stated here rather than a guess made twice.
fn namespace_segments(name: &SgNode) -> Vec<String> {
    if name.kind() == "nested_namespace_specifier" {
        return name
            .children()
            .filter(|c| c.kind() == "namespace_identifier")
            .map(|c| c.text().to_string())
            .collect();
    }
    vec![name.text().to_string()]
}

/// What a declarator names: its qualifier, its own name, and whether it
/// declares a function.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Declared {
    /// Qualifier segments an out-of-line definition wrote: `fmt::detail` in
    /// `void fmt::detail::f() {}`.
    qualifier: Vec<String>,
    /// The declared name, unqualified.
    name: String,
    /// A function declarator sat somewhere in the chain.
    function: bool,
}

/// Unwrap a declarator chain down to the name it declares.
///
/// C++ wraps a declarator in as many layers as the declaration has operators
/// — `*`, `&`, `[]`, `()`, an initialiser — and every one of them is a node
/// this has to walk through rather than around.
fn declared(node: &SgNode) -> Option<Declared> {
    fn walk(node: &SgNode, function: bool) -> Option<Declared> {
        match &*node.kind() {
            "function_declarator" => walk(&node.field("declarator")?, true),
            "pointer_declarator"
            | "reference_declarator"
            | "parenthesized_declarator"
            | "array_declarator"
            | "init_declarator"
            | "attributed_declarator" => {
                // `reference_declarator` states no `declarator` field in some
                // grammar versions, so fall back to the one child that is a
                // declarator rather than punctuation.
                let inner = node
                    .field("declarator")
                    .or_else(|| node.children().find(|c| walk(c, function).is_some()))?;
                walk(&inner, function)
            }
            "qualified_identifier" => {
                let scope = node.field("scope")?.text().to_string();
                let mut inner = walk(&node.field("name")?, function)?;
                inner.qualifier.insert(0, scope);
                Some(inner)
            }
            "template_function" | "template_type" => walk(&node.field("name")?, function),
            "identifier"
            | "field_identifier"
            | "type_identifier"
            | "namespace_identifier"
            | "destructor_name"
            | "operator_name" => Some(Declared {
                qualifier: Vec::new(),
                name: node.text().to_string(),
                function,
            }),
            _ => None,
        }
    }
    walk(node, false)
}

/// The nearest *nameable* enclosing definition of a reference site.
///
/// An `#include` at the top of a file belongs to nothing, and the driver
/// sources it at the file's own unit node; one inside `namespace fmt` belongs
/// to `fmt`.
fn enclosing_definition(node: &SgNode) -> Option<Encloser> {
    let frames = frames(node)?;
    let last = frames.last()?;
    let kind = if last.is_type {
        DefKind::Type
    } else {
        DefKind::Module
    };
    Some(Encloser {
        path: frames.into_iter().map(|f| f.name).collect(),
        kind,
    })
}

/// One definition, with the fields every C++ declaration shares.
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

/// Whether a declaration carries a `static` storage class.
fn is_static(node: &SgNode) -> bool {
    node.children()
        .any(|c| c.kind() == "storage_class_specifier" && c.text() == "static")
}

/// Bytes a header name may be spelled with, for [`defuse_header_names`].
///
/// Deliberately narrower than the standard's *h-char-sequence*, which is
/// anything but `>` and a newline. Narrowing it is what makes the rewrite
/// safe on bytes this pass has not *proved* are a directive: none of `*`,
/// `"`, `(`, `)` or a backslash is in the set, so the scan below stops before
/// it can rewrite away the `*/` of a block comment or the `)delim"` of a raw
/// string whose interior happens to begin a line with `#if`.
fn is_header_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'+' | b'-')
}

/// One logical preprocessing line: the index of the `\n` that ends it, or the
/// length of the input, following backslash-newline splices.
fn logical_line_end(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => return i,
            b'\\' => {
                let mut k = i + 1;
                while matches!(bytes.get(k), Some(b' ' | b'\t' | b'\r')) {
                    k += 1;
                }
                i = if bytes.get(k) == Some(&b'\n') {
                    k + 1
                } else {
                    i + 1
                };
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

/// The byte range of a `#if` or `#elif` condition within `start..end`, or
/// `None` when the line is not one of those two directives.
///
/// `#ifdef` and `#ifndef` are not among them and must not be: they are
/// matched here by the whole directive word, never by a prefix, and neither
/// takes anything but an identifier.
fn conditional_condition(bytes: &[u8], start: usize, end: usize) -> Option<(usize, usize)> {
    let mut i = start;
    while i < end && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'#') {
        return None;
    }
    i += 1;
    while i < end && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    let word = i;
    while i < end && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    matches!(&bytes[word..i], b"if" | b"elif").then_some((i, end))
}

/// Replace `<header/name>` inside a `#if` or `#elif` condition with an
/// equal-length filler, before the file is parsed.
///
/// # The bug this exists for
///
/// `__has_include(<version>)` — and every project macro wrapping it, fmt's
/// `FMT_HAS_INCLUDE` among them — is a preprocessor operator the pinned
/// tree-sitter C++ grammar has no rule for. It reads the `<` and `>` as
/// comparisons, and when the operator is not the whole condition the
/// expression never terminates: the parse runs off the end of the directive
/// and swallows the **rest of the file** into one `ERROR` node. Every
/// `preproc_include` after it stops existing, so the extractor emits no
/// reference for includes that are plainly there. Measured on the corpus,
/// two `#include` directives vanished this way; on a file whose first
/// directive has the shape, *all* of them do.
///
/// That is the one failure mode this project's non-negotiables forbid
/// outright — a reference deleted from the denominator, silently — so it is
/// fixed at the only place a track may fix a grammar it does not own: the
/// bytes handed to it.
///
/// # Why this is not "evaluating the preprocessor"
///
/// Nothing here decides a branch. tree-sitter does not evaluate a `#if`
/// either — both arms are in the tree whatever the condition says — and tier
/// 2 reads no condition at all, so replacing a header name inside one with
/// `0` changes no fact this extractor emits. Every replacement is
/// **length-preserving**, so every [`Span`] in the file is the span it would
/// have been, byte for byte, and the [`quoted_content`] of every `#include`
/// is read off unchanged bytes: an `#include` line is never a condition.
///
/// Untouched files pay a scan and no allocation.
fn defuse_header_names(source: &str) -> Cow<'_, str> {
    let bytes = source.as_bytes();
    let mut out: Option<Vec<u8>> = None;
    let mut pos = 0;
    while pos < bytes.len() {
        let end = logical_line_end(bytes, pos);
        if let Some((from, to)) = conditional_condition(bytes, pos, end) {
            let mut i = from;
            while i < to {
                if bytes[i] == b'<' {
                    let mut k = i + 1;
                    while k < to && is_header_name_byte(bytes[k]) {
                        k += 1;
                    }
                    // A non-empty header name, closed on the same logical
                    // line. `#if A < 3 && B > 1` never matches: the space
                    // after `<` is not a header-name byte, so a genuine
                    // comparison is left exactly as written.
                    if k > i + 1 && k < to && bytes[k] == b'>' {
                        let buf = out.get_or_insert_with(|| bytes.to_vec());
                        buf[i] = b'0';
                        buf[i + 1..=k].fill(b' ');
                        i = k + 1;
                        continue;
                    }
                }
                i += 1;
            }
        }
        pos = end.saturating_add(1);
    }
    match out {
        // Only ASCII bytes are read and only ASCII bytes are written, and an
        // ASCII byte can never be part of a multi-byte sequence, so the
        // rewrite cannot land inside one.
        Some(buf) => Cow::Owned(String::from_utf8(buf).expect("ASCII-for-ASCII keeps UTF-8 valid")),
        None => Cow::Borrowed(source),
    }
}

/// Extract one C++ file. The whole of the extractor's public surface.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<CppLang> {
    static RULES: OnceLock<Rules> = OnceLock::new();
    let rules = RULES.get_or_init(|| Rules::compile(CPP_RULES).expect("cpp.yml compiles"));

    let mut facts: FileFacts<CppLang> = FileFacts {
        header: CppHeader {
            rel_path: rel_path.to_string(),
            includes: Vec::new(),
        },
        defs: Vec::new(),
        refs: Vec::new(),
    };

    // The file's own unit node, first, because the driver reads the first
    // `Module` definition as the file's container. Every file the walk
    // reaches is a unit whether or not it declares anything: an `#include`
    // naming an empty file still resolves.
    let stem = rel_path.rsplit('/').next().unwrap_or(rel_path);
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

    // The grammar is pinned and has no `__has_include`; see
    // [`defuse_header_names`] for what one unparseable condition costs.
    let prepared = defuse_header_names(source);
    let tree = SourceTree::parse_cpp(&prepared);
    for (rule, node) in tree.matches(rules) {
        match rule {
            "include" => include(&mut facts, &node),
            "decl" => declaration(&mut facts, &node),
            "def-namespace" => namespace(&mut facts, &node),
            "def-namespace-alias" => namespace_alias(&mut facts, &node),
            "def-record" => record(&mut facts, &node, DefFacets::default()),
            "def-enum" => record(&mut facts, &node, DefFacets::ENUM),
            "def-enumerator" => enumerator(&mut facts, &node),
            "def-function" => function(&mut facts, &node),
            "def-member" => member(&mut facts, &node),
            "def-typedef" => typedef(&mut facts, &node),
            "def-alias" => alias(&mut facts, &node),
            _ => {}
        }
    }
    // Rules run one at a time, so the records arrive rule-major; source order
    // is what a reader of a report expects and what a span-keyed pairing
    // needs to be stable under.
    facts.defs[1..].sort_by_key(|d| d.span.byte_start);
    facts.refs.sort_by_key(|r| r.span.byte_start);
    facts.header.includes.sort_by_key(|i| i.span.byte_start);
    facts
}

/// `#include "…"`, `#include <…>`, `#include SOME_MACRO`.
fn include(facts: &mut FileFacts<CppLang>, node: &SgNode) {
    let path = node
        .field("path")
        .or_else(|| node.children().find(|c| is_include_path(c)));
    let (form, spelled) = match path {
        Some(p) if p.kind() == "string_literal" => {
            let text = p.text().to_string();
            match quoted_content(&p) {
                Some(spec) => (IncludeForm::Quoted(spec), text),
                None => (IncludeForm::Computed, text),
            }
        }
        Some(p) if p.kind() == "system_lib_string" => {
            let text = p.text().to_string();
            let spec = text
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string();
            (IncludeForm::Angle(spec), text)
        }
        // `#include FMT_HEADER`: the specifier is a macro, and only a
        // preprocessor run knows what it expands to.
        Some(p) => (IncludeForm::Computed, p.text().to_string()),
        None => return,
    };
    emit(facts, node, form, spelled);
}

/// Whether a `preproc_include` child is the thing being included.
fn is_include_path(node: &SgNode) -> bool {
    !matches!(&*node.kind(), "#include" | "\n" | "comment")
}

/// A quoted include's contents, or `None` when it is not one plain literal.
///
/// An escape sequence answers `None`: a specifier this cannot read is one the
/// resolver must refuse to guess at, not one it may approximate.
fn quoted_content(node: &SgNode) -> Option<String> {
    let mut out = String::new();
    for child in node.children() {
        match &*child.kind() {
            "string_content" => out.push_str(&child.text()),
            "\"" => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Record one import clause and the reference that shares its span.
fn emit(facts: &mut FileFacts<CppLang>, node: &SgNode, form: IncludeForm, spelled: String) {
    let span = span_of(node);
    let target = match &form {
        IncludeForm::Quoted(spec) | IncludeForm::Angle(spec) | IncludeForm::Module(spec) => {
            RefTarget {
                root: TargetRoot::Name,
                segments: vec![spec.clone()],
            }
        }
        // The root is not a name: a macro specifier is exactly the shape
        // `TargetRoot::Expr` exists for.
        IncludeForm::Computed => RefTarget {
            root: TargetRoot::Expr,
            segments: Vec::new(),
        },
    };
    facts.header.includes.push(IncludeSpec { form, span });
    facts.refs.push(Reference {
        kind: RefKind::Import,
        space: DeclSpace::Namespace,
        raw_target: spelled,
        target,
        // Tier 2 emits no expression-level reference, so nothing here can
        // name a local: `LocalBinding` does not apply to this track.
        locally_bound: false,
        argc: None,
        enclosing: enclosing_definition(node),
        span,
    });
}

/// A `declaration` node: a module directive the grammar misparsed, a function
/// prototype, or a variable at namespace scope.
fn declaration(facts: &mut FileFacts<CppLang>, node: &SgNode) {
    if module_directive(facts, node) {
        return;
    }
    let Some(frames) = frames(node) else { return };
    let owner: Vec<String> = frames.iter().map(|f| f.name.clone()).collect();
    let static_here = is_static(node);
    let typed = states_type(node);
    for declarator in node.field_children("declarator") {
        let Some(d) = declared(&declarator) else {
            continue;
        };
        let mut owner = owner.clone();
        owner.extend(d.qualifier.iter().cloned());
        let kind = if d.function {
            match function_kind(&frames, &d, typed) {
                Some(kind) => kind,
                None => continue, // a macro invocation, not a declaration
            }
        } else {
            DefKind::Var
        };
        let space = DeclSpace::Value;
        let mut facets = if d.function {
            // A declaration with no body: the entity is declared here and
            // defined somewhere this file does not say.
            DefFacets::ABSTRACT
        } else {
            DefFacets::default()
        };
        if static_here {
            facets = facets.union(DefFacets::STATIC);
        }
        facts
            .defs
            .push(def(kind, d.name, owner, space, facets, span_of(node)));
    }
}

/// What a function-shaped declaration really declares, or `None` when it
/// declares nothing.
///
/// **A macro invocation followed by a braced block is a `function_definition`
/// to this grammar.** `TEST(format_test, escape) { … }` comes back as a
/// function named `TEST` with no return type, and the 33 files the
/// six-extension world read wrote 600 of them. C++ gives every function a declared return type
/// except a constructor, a destructor and a conversion function ([dcl.fct]),
/// so a function-shaped node that states no type and is none of those three
/// is a macro invocation, and no node is invented for it. Merging 600 of them
/// into one `TEST` identity would be the worse half of the same mistake.
///
/// A conversion function — `A::operator bool()` — states no type either, and
/// is a recorded non-claim rather than a guess: its declarator comes back as
/// an `operator_cast` node whose name is the target type, which is a spelling
/// this identity space has no rule for.
fn function_kind(frames: &[Frame], d: &Declared, states_type: bool) -> Option<DefKind> {
    // The type a constructor or destructor would belong to: the final
    // qualifier of an out-of-line definition, or the innermost lexical frame.
    let target = match d.qualifier.last() {
        Some(last) => Some(last.as_str()),
        None => frames.last().filter(|f| f.is_type).map(|f| f.name.as_str()),
    };
    if !states_type {
        // Unambiguous exactly because no type is stated: nothing else in C++
        // may be written this way.
        if target == Some(d.name.as_str()) {
            return Some(DefKind::Constructor);
        }
        if d.name.starts_with('~') {
            return Some(DefKind::Method);
        }
        return None;
    }
    // A qualified declarator is an out-of-line definition: one file cannot
    // say whether the final qualifier is a class or a namespace, so the kind
    // stays the weaker of the two and only the owner path is claimed.
    Some(match frames.last() {
        Some(f) if f.is_type && d.qualifier.is_empty() => DefKind::Method,
        _ => DefKind::Function,
    })
}

/// Whether a declaration states a return or declared type of its own.
fn states_type(node: &SgNode) -> bool {
    node.field("type").is_some()
}

/// A C++20 module directive, read off the token sequence the grammar's lack
/// of module support leaves behind. Answers whether one was found.
fn module_directive(facts: &mut FileFacts<CppLang>, node: &SgNode) -> bool {
    let mut words: Vec<SgNode> = node
        .children()
        .filter(|c| {
            matches!(
                &*c.kind(),
                "identifier" | "type_identifier" | "ERROR" | "namespace_identifier"
            )
        })
        .collect();
    if words.is_empty() {
        return false;
    }
    // `export module fmt;` and `export import std;` put `export` first.
    let exported = words[0].text() == "export";
    if exported {
        words.remove(0);
    }
    let Some(keyword) = words.first().map(|w| w.text().to_string()) else {
        return false;
    };
    if keyword != "module" && keyword != "import" {
        return false;
    }
    let Some(name) = words.get(1).map(|w| w.text().trim().to_string()) else {
        return false;
    };
    // A module name is an identifier, or several joined by dots. Anything
    // else — a partition, a header unit, a stray misparse — is a shape
    // nothing measured, and this build does not guess at one.
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return false;
    }
    if keyword == "module" && exported {
        // `export module fmt;` declares the module this file is the
        // interface unit of. A definition, and the only one in the module
        // identity space.
        facts.defs.push(def(
            DefKind::Module,
            name,
            Vec::new(),
            DeclSpace::Namespace,
            DefFacets::EXPORTED,
            span_of(node),
        ));
    } else {
        // `import fmt;`, `import std;`, and `module fmt;` in an
        // implementation unit: all three name a module declared elsewhere.
        emit(
            facts,
            node,
            IncludeForm::Module(name),
            node.text().trim().to_string(),
        );
    }
    true
}

/// `namespace fmt { … }`, including `namespace fmt::inline v11 { … }`.
fn namespace(facts: &mut FileFacts<CppLang>, node: &SgNode) {
    let Some(frames) = frames(node) else { return };
    let Some(name) = node.field("name") else {
        return; // an unnamed namespace names nothing
    };
    let mut owner: Vec<String> = frames.iter().map(|f| f.name.clone()).collect();
    let segments = namespace_segments(&name);
    let Some((last, prefix)) = segments.split_last() else {
        return;
    };
    owner.extend(prefix.iter().cloned());
    facts.defs.push(def(
        DefKind::Module,
        last.clone(),
        owner,
        DeclSpace::Namespace,
        DefFacets::default(),
        span_of(node),
    ));
}

/// `namespace ns = fmt::detail;`
fn namespace_alias(facts: &mut FileFacts<CppLang>, node: &SgNode) {
    let Some(frames) = frames(node) else { return };
    let Some(name) = node.field("name") else {
        return;
    };
    facts.defs.push(def(
        DefKind::Alias,
        name.text().to_string(),
        frames.iter().map(|f| f.name.clone()).collect(),
        DeclSpace::Namespace,
        DefFacets::default(),
        span_of(node),
    ));
}

/// `class C { … }`, `struct S { … }`, `union U { … }`, `enum class E { … }`.
fn record(facts: &mut FileFacts<CppLang>, node: &SgNode, facets: DefFacets) {
    let Some(frames) = frames(node) else { return };
    let Some(name) = node.field("name") else {
        return; // an anonymous type names nothing
    };
    facts.defs.push(def(
        DefKind::Type,
        name.text().to_string(),
        frames.iter().map(|f| f.name.clone()).collect(),
        DeclSpace::Type,
        facets,
        span_of(node),
    ));
}

/// One `enum` constant. Owned by the enumeration, which is how C++11 and
/// later spell it whether the enumeration is scoped or not.
fn enumerator(facts: &mut FileFacts<CppLang>, node: &SgNode) {
    let Some(frames) = frames(node) else { return };
    let Some(name) = node.field("name") else {
        return;
    };
    facts.defs.push(def(
        DefKind::Const,
        name.text().to_string(),
        frames.iter().map(|f| f.name.clone()).collect(),
        DeclSpace::Value,
        DefFacets::default(),
        span_of(node),
    ));
}

/// A function with a body, in or out of a class.
fn function(facts: &mut FileFacts<CppLang>, node: &SgNode) {
    let Some(frames) = frames(node) else { return };
    let Some(declarator) = node.field("declarator") else {
        return;
    };
    let Some(d) = declared(&declarator) else {
        return;
    };
    if !d.function {
        return;
    }
    let mut owner: Vec<String> = frames.iter().map(|f| f.name.clone()).collect();
    owner.extend(d.qualifier.iter().cloned());
    let facets = if is_static(node) {
        DefFacets::STATIC
    } else {
        DefFacets::default()
    };
    let Some(kind) = function_kind(&frames, &d, states_type(node)) else {
        return; // a macro invocation, not a declaration
    };
    facts.defs.push(def(
        kind,
        d.name,
        owner,
        DeclSpace::Value,
        facets,
        span_of(node),
    ));
}

/// A class member. Only member *functions* are filed: a data member is not a
/// definition at tier 2, because nothing at tier 2 names one.
fn member(facts: &mut FileFacts<CppLang>, node: &SgNode) {
    let Some(frames) = frames(node) else { return };
    let owner: Vec<String> = frames.iter().map(|f| f.name.clone()).collect();
    let static_here = is_static(node);
    let typed = states_type(node);
    for declarator in node.field_children("declarator") {
        let Some(d) = declared(&declarator) else {
            continue;
        };
        if !d.function {
            continue;
        }
        let Some(kind) = function_kind(&frames, &d, typed) else {
            continue; // a macro invocation, not a declaration
        };
        let mut owner = owner.clone();
        owner.extend(d.qualifier.iter().cloned());
        let mut facets = DefFacets::ABSTRACT;
        if static_here {
            facets = facets.union(DefFacets::STATIC);
        }
        facts.defs.push(def(
            kind,
            d.name,
            owner,
            DeclSpace::Value,
            facets,
            span_of(node),
        ));
    }
}

/// `typedef int myint;`
fn typedef(facts: &mut FileFacts<CppLang>, node: &SgNode) {
    let Some(frames) = frames(node) else { return };
    let owner: Vec<String> = frames.iter().map(|f| f.name.clone()).collect();
    let declarators: Vec<SgNode> = node.field_children("declarator").collect();
    let declarators = if declarators.is_empty() {
        node.children()
            .filter(|c| c.kind() == "type_identifier")
            .collect()
    } else {
        declarators
    };
    for declarator in declarators {
        let Some(d) = declared(&declarator) else {
            continue;
        };
        facts.defs.push(def(
            DefKind::Alias,
            d.name,
            owner.clone(),
            DeclSpace::Type,
            DefFacets::default(),
            span_of(node),
        ));
    }
}

/// `using alias_t = double;`
fn alias(facts: &mut FileFacts<CppLang>, node: &SgNode) {
    let Some(frames) = frames(node) else { return };
    let Some(name) = node.field("name") else {
        return;
    };
    facts.defs.push(def(
        DefKind::Alias,
        name.text().to_string(),
        frames.iter().map(|f| f.name.clone()).collect(),
        DeclSpace::Type,
        DefFacets::default(),
        span_of(node),
    ));
}

/// The C++ extractor, as the driver holds it.
pub struct CppExtractor;

impl Extractor<CppLang> for CppExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<CppLang> {
        extract(rel_path, source)
    }
}
