//! Ruby extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/ruby.yml`) select nodes by kind; this
//! module interprets their fields.
//!
//! # What a tier-2 extractor emits, and what it must not
//!
//! Definitions and structure, plus **import references and nothing else**.
//! Ruby's gate is an import-resolution rate, so a call site or a `include
//! Helpers` emitted here would enter a denominator nothing in this track can
//! resolve — tier-1 coverage claimed without tier-1 measurement. `class C <
//! Base` is therefore read as part of `C`'s structure and produces no
//! [`RefKind::Inherit`] reference, and no call site produces a
//! [`RefKind::Call`] one.
//!
//! # Recorded under-counts
//!
//! Each is a known shortfall, written down rather than left to be
//! rediscovered, and none may be closed by widening a bucket:
//!
//! - **A module's own `autoload`.** `Foo.autoload :Bar, "path"` names a file
//!   on the load path exactly as the receiverless form does, but its receiver
//!   may be any constant and reading one would mean deciding which receivers
//!   are `Kernel` in disguise. Only `Kernel` itself is read — it *is* the
//!   receiverless form, spelled out — and every other receiver contributes no
//!   reference rather than a guessed one.
//! - **`alias`, `alias_method` and `define_method`.** All three declare a
//!   real method; the last takes its name from an expression, and the first
//!   two would need the definition they forward to.
//! - **Instance variables.** `@env = env` is an assignment in a method body,
//!   not a declaration site, and one node per assignment would be several
//!   nodes for one slot.
//! - **A declaration inside a block.** `Struct.new do def x; end end` really
//!   does define a method, on a receiver no lexical constant path names. The
//!   owner is not derivable from the file, so no node is invented for it.
//! - **The constant an `autoload` binds.** `autoload :Builder, "rack/builder"`
//!   declares `Rack::Builder` as well as naming a file. The file is the
//!   reference; the constant is left to the definition site the loaded file
//!   already carries, which is the same FQN.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_ruby::lang::RubyLang;

/// The embedded Ruby extraction rules.
const RUBY_RULES: &str = include_str!("../rules/ruby.yml");

/// How an import clause spells the file it names.
///
/// The distinction is the whole of Ruby's import model: `require_relative`
/// resolves against the requiring file, `require` and `autoload` resolve
/// against the load path, and a specifier that is not a literal resolves
/// against nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportForm {
    /// `require_relative <literal>` — relative to the requiring file.
    Relative(String),
    /// `require <literal>` or `autoload :C, <literal>` — relative to each
    /// entry of the load path, in order.
    LoadPath(String),
    /// The specifier is not a string literal: interpolated, computed, or a
    /// name. Never guessed.
    Dynamic,
}

/// One import clause: what it spells plus where it sits.
///
/// Every `ImportSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is how the
/// resolver pairs the two without the core learning what a `require` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    /// What the clause spells.
    pub form: ImportForm,
    /// Where the clause sits. The whole call, so the key is unique.
    pub span: Span,
}

/// Per-file Ruby facts only the Ruby resolver reads.
///
/// `rel_path` is here for the same reason Go's and Python's are: a
/// `require_relative` is resolved against *where the file is*, and the core
/// must not be the layer that turns a path into a load target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RubyHeader {
    /// Repo-relative, `/`-separated path of the file.
    pub rel_path: String,
    /// Every import clause, in source order.
    pub imports: Vec<ImportSpec>,
}

/// One lexical frame between a node and the top of its file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Frame {
    /// `module M` or `class C`, with the name as written.
    Const {
        /// The constant path as written, leading `::` stripped.
        name: String,
        /// The name began with `::`, so it restarts the path at the top.
        absolute: bool,
        /// `class`, rather than `module`.
        is_class: bool,
    },
    /// `class << self`: everything below declares on the singleton.
    Singleton,
    /// `def m` or `def self.m`.
    Method {
        /// The method's own name.
        name: String,
        /// Declared with an explicit singleton receiver.
        singleton: bool,
    },
}

/// The lexical frames enclosing a node, outermost first.
///
/// `None` when a frame is crossed that no lexical path can name: a block,
/// whose receiver is a runtime value, or `class << obj` for an object that is
/// not `self`. Answering `None` is what keeps a node from being invented for
/// a declaration whose owner this file does not state.
fn frames(node: &SgNode) -> Option<Vec<Frame>> {
    let mut out = Vec::new();
    for a in node.ancestors() {
        match &*a.kind() {
            "module" | "class" => {
                let raw = a.field("name")?.text().to_string();
                let absolute = raw.starts_with("::");
                out.push(Frame::Const {
                    name: raw.trim_start_matches("::").to_string(),
                    absolute,
                    is_class: a.kind() == "class",
                });
            }
            "singleton_class" => {
                if a.field("value").map(|v| v.kind().to_string()).as_deref() != Some("self") {
                    return None; // `class << obj`: the owner is a value
                }
                out.push(Frame::Singleton);
            }
            "method" | "singleton_method" => {
                let name = a.field("name")?.text().to_string();
                out.push(Frame::Method {
                    name,
                    singleton: a.kind() == "singleton_method",
                });
            }
            "block" | "do_block" | "lambda" => return None,
            _ => {}
        }
    }
    out.reverse();
    Some(out)
}

/// The constant path the frames spell, outermost first, and whether the
/// innermost frame is a singleton opener.
///
/// `None` when a method frame is crossed: a declaration inside a method body
/// has no lexical owner this file states.
fn owner_of(frames: &[Frame]) -> Option<(Vec<String>, bool)> {
    let mut path: Vec<String> = Vec::new();
    let mut singleton = false;
    for frame in frames {
        match frame {
            Frame::Const {
                name,
                absolute,
                is_class: _,
            } => {
                if *absolute {
                    path.clear();
                }
                path.push(name.clone());
                singleton = false;
            }
            Frame::Singleton => singleton = true,
            Frame::Method { .. } => return None,
        }
    }
    Some((path, singleton))
}

/// The nearest *nameable* enclosing definition of a reference site.
///
/// A `require` inside a method belongs to that method; one inside `module
/// Rack` belongs to `Rack`; one at the top of a file belongs to nothing, and
/// the driver sources it at the file's own feature node.
///
/// A singleton method's name is carried as `self.<name>`, which is exactly
/// how [`crate::track_ruby::resolve`] spells it back — so the identity an
/// edge starts at and the identity the definition was filed under agree.
fn enclosing_definition(node: &SgNode) -> Option<Encloser> {
    let frames = frames(node)?;
    let (name, singleton) = match frames.last()? {
        Frame::Method { name, singleton } => (name.clone(), *singleton),
        Frame::Const { .. } | Frame::Singleton => {
            let consts = &frames[..frames.len()];
            let (path, _) = owner_of(consts)?;
            if path.is_empty() {
                return None;
            }
            let is_class = matches!(
                frames.iter().rev().find_map(|f| match f {
                    Frame::Const { is_class, .. } => Some(*is_class),
                    _ => None,
                }),
                Some(true)
            );
            return Some(Encloser {
                path,
                kind: if is_class {
                    DefKind::Type
                } else {
                    DefKind::Module
                },
            });
        }
    };
    let (mut path, opener_singleton) = owner_of(&frames[..frames.len() - 1])?;
    let kind = if path.is_empty() {
        DefKind::Function
    } else {
        DefKind::Method
    };
    if singleton || opener_singleton {
        path.push(format!("self.{name}"));
    } else {
        path.push(name);
    }
    Some(Encloser { path, kind })
}

/// A string literal's value, or `None` when it is not one plain literal.
///
/// `pub(crate)` because phase 0 reads the same shape out of a gemspec — one
/// reader for "is this argument a literal string?", not two that can drift.
///
/// Interpolation, an escape sequence and a computed argument all answer
/// `None`: a specifier this function cannot read is one the resolver must
/// refuse to guess, not one it may approximate.
pub(crate) fn string_literal(node: &SgNode) -> Option<String> {
    match &*node.kind() {
        "string" => {
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
        // `require "rack/" "utils"` is one literal written in two pieces.
        "chained_string" => {
            let mut out = String::new();
            for child in node.children() {
                out.push_str(&string_literal(&child)?);
            }
            Some(out)
        }
        _ => None,
    }
}

/// A call's arguments, punctuation dropped.
pub(crate) fn arg_nodes<'r>(call: &SgNode<'r>) -> Vec<SgNode<'r>> {
    let Some(list) = call.field("arguments") else {
        return Vec::new();
    };
    list.children()
        .filter(|c| !matches!(&*c.kind(), "(" | ")" | ","))
        .collect()
}

/// The symbol a `:name` argument names.
fn symbol_name(node: &SgNode) -> Option<String> {
    (node.kind() == "simple_symbol").then(|| node.text().trim_start_matches(':').to_string())
}

/// One definition, with the fields every Ruby declaration shares.
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

/// Extract one Ruby file. The whole of the extractor's public surface.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<RubyLang> {
    static RULES: OnceLock<Rules> = OnceLock::new();
    let rules = RULES.get_or_init(|| Rules::compile(RUBY_RULES).expect("ruby.yml compiles"));

    let mut facts: FileFacts<RubyLang> = FileFacts {
        header: RubyHeader {
            rel_path: rel_path.to_string(),
            imports: Vec::new(),
        },
        defs: Vec::new(),
        refs: Vec::new(),
    };

    // The file's own feature node, first, because the driver reads the first
    // `Module` definition as the file's container. Every `.rb` file is a
    // feature whether or not it declares a constant: a `require_relative`
    // naming an empty file still resolves.
    let stem = rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path)
        .strip_suffix(".rb")
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

    let tree = SourceTree::parse_ruby(source);
    for (rule, node) in tree.matches(rules) {
        match rule {
            "def-module" | "def-class" => declaration(&mut facts, &node, rule == "def-class"),
            "def-method" | "def-singleton-method" => method(&mut facts, &node),
            "assign" | "opassign" => constant_assignment(&mut facts, &node),
            "call" => call(&mut facts, &node),
            _ => {}
        }
    }
    // Rules run one at a time, so the records arrive rule-major; source order
    // is what a reader of a report expects and what a span-keyed pairing
    // needs to be stable under.
    facts.defs[1..].sort_by_key(|d| d.span.byte_start);
    facts.refs.sort_by_key(|r| r.span.byte_start);
    facts.header.imports.sort_by_key(|i| i.span.byte_start);
    facts
}

/// `module M` / `class C`.
fn declaration(facts: &mut FileFacts<RubyLang>, node: &SgNode, is_class: bool) {
    let Some(frames) = frames(node) else { return };
    let Some((mut owner, _)) = owner_of(&frames) else {
        return;
    };
    let Some(raw) = node.field("name").map(|n| n.text().to_string()) else {
        return;
    };
    if raw.starts_with("::") {
        owner.clear();
    }
    facts.defs.push(def(
        if is_class {
            DefKind::Type
        } else {
            DefKind::Module
        },
        raw.trim_start_matches("::").to_string(),
        owner,
        if is_class {
            DeclSpace::Type
        } else {
            DeclSpace::Namespace
        },
        DefFacets::default(),
        span_of(node),
    ));
}

/// `def m` / `def self.m` / `def obj.m`.
fn method(facts: &mut FileFacts<RubyLang>, node: &SgNode) {
    let Some(frames) = frames(node) else { return };
    let Some((mut owner, opener_singleton)) = owner_of(&frames) else {
        return;
    };
    let Some(name) = node.field("name").map(|n| n.text().to_string()) else {
        return;
    };
    let mut singleton = opener_singleton;
    if let Some(object) = node.field("object") {
        match &*object.kind() {
            "self" => singleton = true,
            // `def Rack.foo` declares on the constant it names.
            "constant" | "scope_resolution" => {
                let raw = object.text().to_string();
                if raw.starts_with("::") {
                    owner.clear();
                }
                owner.push(raw.trim_start_matches("::").to_string());
                singleton = true;
            }
            // `def obj.foo`: the owner is a runtime value, so there is no
            // node to file this under and none is invented.
            _ => return,
        }
    }
    let kind = if owner.is_empty() {
        DefKind::Function
    } else {
        DefKind::Method
    };
    let facets = if singleton {
        DefFacets::STATIC
    } else {
        DefFacets::default()
    };
    facts.defs.push(def(
        kind,
        name,
        owner,
        DeclSpace::Value,
        facets,
        span_of(node),
    ));
}

/// `CONST = …` and `CONST ||= …`, including `A::B = …`.
///
/// An assignment to a lowercase name binds a local variable, which is not a
/// node by decision, and an assignment to `@ivar` writes a slot rather than
/// declaring one — neither produces a definition.
fn constant_assignment(facts: &mut FileFacts<RubyLang>, node: &SgNode) {
    let Some(left) = node.field("left") else {
        return;
    };
    let raw = match &*left.kind() {
        "constant" | "scope_resolution" => left.text().to_string(),
        _ => return,
    };
    let Some(frames) = frames(node) else { return };
    let Some((mut owner, _)) = owner_of(&frames) else {
        return;
    };
    if raw.starts_with("::") {
        owner.clear();
    }
    let mut parts: Vec<String> = raw
        .trim_start_matches("::")
        .split("::")
        .map(str::to_string)
        .collect();
    let Some(name) = parts.pop() else { return };
    owner.extend(parts);
    facts.defs.push(def(
        DefKind::Const,
        name,
        owner,
        DeclSpace::Value,
        DefFacets::default(),
        span_of(node),
    ));
}

/// `Kernel`, spelled either way: the one receiver an import may carry.
///
/// `Kernel.require 'time'` is the same site as `require 'time'` — `require`
/// is `Kernel`'s own method, and the qualified form names it explicitly. Any
/// other receiver is a runtime value or another module's `autoload`, and
/// neither is this extractor's import.
fn is_kernel(node: &SgNode) -> bool {
    matches!(&*node.kind(), "constant" | "scope_resolution")
        && node.text().trim_start_matches("::") == "Kernel"
}

/// A call this extractor reads: the three that import, and the three that
/// declare.
fn call(facts: &mut FileFacts<RubyLang>, node: &SgNode) {
    // A declaring call is receiverless by construction: `attr_reader` writes
    // on the enclosing module, and `C.attr_reader` is not a shape this reads.
    // An importing call may name `Kernel` and nothing else.
    let receiver = node.field("receiver");
    if let Some(r) = &receiver
        && !is_kernel(r)
    {
        return;
    }
    let Some(method) = node.field("method") else {
        return;
    };
    if method.kind() != "identifier" {
        return;
    }
    let args = arg_nodes(node);
    match &*method.text() {
        "require" => import(facts, node, &args, 0, false),
        "require_relative" => import(facts, node, &args, 0, true),
        // `autoload :Const, "path"`: the constant is the binding, the string
        // is the file, and the string is what resolves.
        "autoload" => import(facts, node, &args, 1, false),
        "attr_reader" | "attr_writer" | "attr_accessor" if receiver.is_none() => {
            attributes(facts, node, &args)
        }
        _ => {}
    }
}

/// One `require`, `require_relative` or `autoload` clause and its reference.
///
/// `at` is which argument spells the file: the first for `require` and
/// `require_relative`, the second for `autoload`.
fn import(
    facts: &mut FileFacts<RubyLang>,
    call: &SgNode,
    args: &[SgNode],
    at: usize,
    relative: bool,
) {
    let Some(specifier) = args.get(at) else {
        return; // `require` with no argument is not an import site
    };
    let span = span_of(call);
    let literal = string_literal(specifier);
    let form = match (&literal, relative) {
        (Some(path), true) => ImportForm::Relative(path.clone()),
        (Some(path), false) => ImportForm::LoadPath(path.clone()),
        (None, _) => ImportForm::Dynamic,
    };
    // The literal text at the site, which is what a `RefKey` is keyed on and
    // what a query prints back. Every argument, not just the one that
    // resolves: `autoload :ForwardRequest, "rack/recursive"` and `autoload
    // :Recursive, "rack/recursive"` name one file from two lines, and a key
    // built from the file alone would merge them into a single row and lose
    // the second site.
    let method = call
        .field("method")
        .map(|m| m.text().to_string())
        .unwrap_or_default();
    let written = match call.field("receiver") {
        Some(r) => format!("{}.{method}", r.text()),
        None => method,
    };
    let spelled: Vec<String> = args.iter().map(|a| a.text().to_string()).collect();
    let target = match &literal {
        Some(path) => RefTarget {
            root: TargetRoot::Name,
            segments: vec![path.clone()],
        },
        // The root is not a name: a computed specifier is exactly the shape
        // `TargetRoot::Expr` exists for.
        None => RefTarget {
            root: TargetRoot::Expr,
            segments: Vec::new(),
        },
    };
    facts.header.imports.push(ImportSpec { form, span });
    facts.refs.push(Reference {
        kind: RefKind::Import,
        space: DeclSpace::Namespace,
        raw_target: format!("{written} {}", spelled.join(", ")),
        target,
        // Tier 2 emits no expression-level reference, so nothing here can
        // name a local: `LocalBinding` does not apply to this track.
        locally_bound: false,
        argc: None,
        enclosing: enclosing_definition(call),
        span,
    });
}

/// `attr_reader :a, :b` and friends: one property per symbol.
fn attributes(facts: &mut FileFacts<RubyLang>, node: &SgNode, args: &[SgNode]) {
    let Some(frames) = frames(node) else { return };
    let Some((owner, singleton)) = owner_of(&frames) else {
        return;
    };
    if owner.is_empty() {
        return; // `attr_reader` at the top of a file declares on Object
    }
    let facets = if singleton {
        DefFacets::STATIC.union(DefFacets::SYNTHETIC)
    } else {
        DefFacets::SYNTHETIC
    };
    for arg in args {
        let Some(name) = symbol_name(arg) else {
            continue;
        };
        facts.defs.push(def(
            DefKind::Property,
            name,
            owner.clone(),
            DeclSpace::Value,
            facets,
            span_of(arg),
        ));
    }
}

/// The Ruby extractor, as the driver holds it.
pub struct RubyExtractor;

impl Extractor<RubyLang> for RubyExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<RubyLang> {
        extract(rel_path, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_compile() {
        Rules::compile(RUBY_RULES).expect("ruby.yml compiles");
    }

    #[test]
    fn a_broken_file_still_yields_its_feature_node() {
        // tree-sitter is error-tolerant, and a file that does not parse is
        // still a file a `require_relative` can name.
        let facts = extract("lib/broken.rb", "class ((( \n");
        assert_eq!(facts.defs[0].kind, DefKind::Module);
        assert_eq!(facts.defs[0].name, "broken");
    }

    #[test]
    fn a_declaration_inside_a_block_has_no_lexical_owner() {
        let facts = extract(
            "lib/app.rb",
            "Struct.new do\n  def x; end\n  CONST = 1\nend\n",
        );
        assert_eq!(facts.defs.len(), 1, "{:?}", facts.defs);
    }

    #[test]
    fn a_singleton_class_over_an_object_names_nothing() {
        let facts = extract(
            "lib/app.rb",
            "obj = X.new\nclass << obj\n  def y; end\nend\n",
        );
        assert_eq!(facts.defs.len(), 1, "{:?}", facts.defs);
    }

    #[test]
    fn an_absolute_constant_restarts_the_path() {
        let facts = extract("lib/app.rb", "module M\n  class ::Top\n  end\nend\n");
        let top = facts.defs.iter().find(|d| d.name == "Top").expect("Top");
        assert!(top.owner.is_empty(), "{:?}", top.owner);
    }

    #[test]
    fn records_come_out_in_source_order() {
        let facts = extract(
            "lib/app.rb",
            "require 'a'\nmodule M\n  X = 1\n  def self.go; end\nend\nrequire 'b'\n",
        );
        let lines: Vec<u32> = facts.refs.iter().map(|r| r.span.line).collect();
        assert_eq!(lines, [1, 6]);
        assert!(
            facts
                .defs
                .windows(2)
                .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
            "{:?}",
            facts.defs,
        );
    }
}
