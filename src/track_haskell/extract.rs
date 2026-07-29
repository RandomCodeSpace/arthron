//! Haskell extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/haskell.yml`) select nodes by kind; this
//! module interprets their fields.
//!
//! # What a best-effort tier-2 extractor emits, and what it must not
//!
//! Definitions and structure, plus **import references and nothing else**.
//! Haskell's gate is an import-resolution rate, so a call site or a type use
//! emitted here would enter a denominator nothing in this track resolves —
//! tier-1 coverage claimed without tier-1 measurement. `instance ToJSON Bool`
//! is therefore read as structure this track does not model and produces no
//! reference at all, and no application of a function produces a
//! [`RefKind::Call`] one.
//!
//! # Which declarations are nameable
//!
//! Exactly two positions: a **top-level** declaration, whose parent is the
//! file's own `declarations` node, and a **class member**, whose parent is the
//! `class_declarations` of a top-level `class`. Everything else —
//! a `where` clause, a `let`, an instance body — binds a name only its own
//! scope can spell, and [`placement`] answers `None` for it rather than
//! inventing an owner. That single check is what keeps locals out of the
//! graph without a single rule about locals.
//!
//! Data constructors, record fields and class methods are filed **under the
//! declaration they belong to**, not under the module: Haskell's type and
//! value namespaces are disjoint and `newtype Key = Key { unKey :: Text }`
//! spends one word on both, so a flat name would merge the type and the
//! constructor into one identity. See [`crate::track_haskell::lang`] for the
//! grammar.
//!
//! # Recorded under-counts
//!
//! Each is a known shortfall, written down rather than left to be
//! rediscovered, and none may be closed by widening a bucket:
//!
//! - **Template Haskell splices.** `$(deriveJSON defaultOptions ''Foo)` really
//!   does declare instances and functions; expanding one means running the
//!   compiler, which this build does not do. The declarations it would add are
//!   absent, and [`crate::UnresolvedReason::Generated`] is not reported
//!   either, because a splice is a *declaration* site and no reference names
//!   what it would have produced.
//! - **Instance bodies.** `instance ToJSON Bool where toJSON = …` binds a
//!   name the class already declared. Nothing new is nameable, so nothing is
//!   emitted; an instance head is chosen by type-directed dispatch, which is
//!   not a name any reference spells.
//! - **`deriving` clauses and standalone `deriving instance`.** Both create
//!   instances, and instances are not nodes here for the reason above.
//! - **An export list's `module M` clause.** `module UnitTests.OptionalFields.Common
//!   ( module Data.Aeson, … )` re-exports a module by name — eight such
//!   clauses in the measured corpus, in two files. They are left out
//!   deliberately: the contract for this track is *import*-like references,
//!   the module each clause names is one the same file already imports, and
//!   the first clause of that list names the file's own module, which would
//!   put an edge from a module to itself into the graph. Seven are that one
//!   list; the eighth is a lone `module X` in another file, re-exporting an
//!   *alias* — `X` is bound by that file's own `import … as X` declarations
//!   and names no module at all, so there is nothing for a reference to
//!   spell. The count is pinned in the corpus acceptance so the omission
//!   stays a measured number.
//! - **`type` synonyms are `DefKind::Type`, not `DefKind::Alias`.** A synonym
//!   really does alias, but it aliases a type *expression* rather than a
//!   single declaration, and this track resolves no type use — so an alias
//!   node here would forward to nothing and claim a link it never made.
//! - **Every arm of a CPP conditional after the first.** Measured against the
//!   pinned tree-sitter-haskell 0.23.1: a `#if`/`#ifdef` line is a `cpp` node
//!   *beside* what it guards, so the first arm parses as ordinary source — but
//!   `#else` and every line under it up to `#endif` are swallowed into a
//!   single `cpp` node, declarations and imports alike. The measured corpus
//!   has 154 CPP directive lines and pays **11 import lines** for this, in
//!   five files, out of 1,085. The number is pinned in the corpus acceptance
//!   so the shortfall stays a measurement; the alternative is preprocessing,
//!   which means choosing a GHC version and reporting a build nobody reads.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, RefKind, RefTarget, Reference, Span, TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_haskell::lang::HsLang;

/// The embedded Haskell extraction rules.
const HASKELL_RULES: &str = include_str!("../rules/haskell.yml");

/// The module name a file with no header declares.
///
/// Haskell 2010 §5.1: an abbreviated module is `module Main(main) where`. Six
/// files in the measured corpus write the header out longhand and mean exactly
/// this, which is why the name is a constant here rather than a special case.
pub const IMPLICIT_MODULE: &str = "Main";

/// One `import` declaration: the module it names plus where it sits.
///
/// `qualified`, `as` and `hiding` are absent on purpose. All three change what
/// the *file* may write afterwards and none changes what is being named, so
/// they are not facts the resolver reads. The selector list is absent for a
/// stronger reason: its entries name values and types inside the module, and
/// resolving those is tier-1 work this track does not claim.
///
/// Every `ImportSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is how the
/// resolver pairs the two without the core learning what a Haskell import is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    /// The dotted module name, as written.
    pub module_name: String,
    /// Where the declaration sits. The whole `import`, so the key is unique.
    pub span: Span,
}

/// Per-file Haskell facts only the Haskell resolver reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HsHeader {
    /// Repo-relative, `/`-separated path of the file. A module's identity, and
    /// what an import's candidate path is compared against.
    pub rel_path: String,
    /// The name the file's `module` header declares, when it writes one.
    ///
    /// `None` is not the same as `Some("Main")`: the first says the file wrote
    /// no header, the second that it wrote one saying `Main`. Six files in the
    /// measured corpus are the second.
    pub module_name: Option<String>,
    /// Every import declaration, in source order.
    pub imports: Vec<ImportSpec>,
}

impl HsHeader {
    /// The module name this file declares, header or not.
    pub fn declared_name(&self) -> &str {
        self.module_name.as_deref().unwrap_or(IMPLICIT_MODULE)
    }
}

/// Where a declaration sits, as the owner chain its definitions carry.
///
/// `Some(vec![])` for a top-level declaration, `Some(vec![class])` for a
/// member of a top-level class, and `None` for everything else — a `where`
/// binding, a `let`, an instance body, a `data instance`'s inner type. `None`
/// is the answer that keeps a name no other file can spell out of the graph.
pub fn placement(node: &SgNode) -> Option<Vec<String>> {
    let mut up = node.ancestors();
    let parent = up.next()?;
    match &*parent.kind() {
        "declarations" => up
            .next()
            .filter(|g| g.kind() == "haskell")
            .map(|_| Vec::new()),
        "class_declarations" => {
            let class = up.next().filter(|c| c.kind() == "class")?;
            up.next().filter(|d| d.kind() == "declarations")?;
            up.next().filter(|g| g.kind() == "haskell")?;
            Some(vec![decl_name(&class.field("name")?)?])
        }
        _ => None,
    }
}

/// The name a declaration node writes, with an operator's parentheses removed.
///
/// `(.:) = explicitParseField` declares `.:`, and `import Data.Aeson ((.:))`
/// writes the same name inside parentheses. One spelling, chosen here, so that
/// the identity a definition is filed under and the one a reader writes agree.
///
/// `None` when the declaration names nothing a reference could spell: `()` and
/// `[]` as type constructors have no name of their own.
fn decl_name(node: &SgNode) -> Option<String> {
    match &*node.kind() {
        "prefix_id" => node
            .children()
            .find(|c| {
                matches!(
                    &*c.kind(),
                    "operator" | "constructor_operator" | "qualified"
                )
            })
            .map(|c| c.text().to_string()),
        "unit" | "prefix_list" | "special" | "empty_list" => None,
        _ => {
            let text = node.text().to_string();
            (!text.is_empty()).then_some(text)
        }
    }
}

/// Every name a declaration's `name`/`names` fields state.
///
/// `x, y :: Int` is one `signature` naming two values and `MkBool, MkOther ::
/// Bool -> GADT Bool` is one GADT constructor naming two, so a reader that
/// took only the first field would lose one declaration per binding list.
fn decl_names<'r>(node: &SgNode<'r>) -> Vec<SgNode<'r>> {
    let list = if node.kind() == "binding_list" {
        Some(node.clone())
    } else {
        node.field("names")
    };
    if let Some(list) = list {
        return list
            .children()
            .filter(|c| {
                matches!(
                    &*c.kind(),
                    "variable" | "constructor" | "prefix_id" | "name"
                )
            })
            .collect();
    }
    node.field("name").into_iter().collect()
}

/// Every record field name a constructor declares.
///
/// The walk descends only through the shapes a field list is written in —
/// `record`, `fields`, and the `prefix` a GADT record constructor wraps its
/// fields in — so it never wanders into a field's *type*, where a name would
/// be a type use this track does not emit.
fn field_names<'r>(ctor: &SgNode<'r>) -> Vec<SgNode<'r>> {
    let mut out = Vec::new();
    let mut stack: Vec<SgNode<'r>> = ctor.children().collect();
    while let Some(node) = stack.pop() {
        match &*node.kind() {
            "record" | "fields" | "prefix" => stack.extend(node.children()),
            "field" => out.extend(node.children().filter(|c| c.kind() == "field_name")),
            _ => {}
        }
    }
    out
}

/// One definition, with the fields every Haskell declaration shares.
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

/// Extract one Haskell file. The whole of the extractor's public surface.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<HsLang> {
    static RULES: OnceLock<Rules> = OnceLock::new();
    let rules = RULES.get_or_init(|| Rules::compile(HASKELL_RULES).expect("haskell.yml compiles"));

    let tree = SourceTree::parse_haskell(source);
    let matches = tree.matches(rules);

    // The header first, because the driver reads a file's first `Module`
    // definition as the container its references are sourced at, and because
    // every `.hs` file the walk reaches is a module whether or not it writes
    // a header.
    let header = matches.iter().find(|(rule, _)| *rule == "header");
    let module_name = header
        .and_then(|(_, node)| node.field("module"))
        .and_then(|m| decl_name(&m));
    let module_span = header.map(|(_, node)| span_of(node)).unwrap_or(Span {
        byte_start: 0,
        byte_end: source.len() as u32,
        line: 1,
    });

    let mut facts: FileFacts<HsLang> = FileFacts {
        header: HsHeader {
            rel_path: rel_path.to_string(),
            module_name: module_name.clone(),
            imports: Vec::new(),
        },
        defs: Vec::new(),
        refs: Vec::new(),
    };
    facts.defs.push(def(
        DefKind::Module,
        module_name.unwrap_or_else(|| IMPLICIT_MODULE.to_string()),
        Vec::new(),
        DeclSpace::Namespace,
        DefFacets::SYNTHETIC,
        module_span,
    ));

    for (rule, node) in &matches {
        match *rule {
            "import" => import(&mut facts, node),
            "def-data" => data_declaration(&mut facts, node),
            "def-data-instance" => data_instance(&mut facts, node),
            "def-type" => type_declaration(&mut facts, node),
            "def-class" => class_declaration(&mut facts, node),
            "def-value" => value_declaration(&mut facts, node),
            "def-pattern" => pattern_synonym(&mut facts, node),
            "def-foreign" => foreign_import(&mut facts, node),
            _ => {}
        }
    }
    // Rules run one at a time, so the records arrive rule-major; source order
    // is what a reader of a report expects and what a span-keyed pairing needs
    // to be stable under. The module node stays at index 0 whatever its span.
    facts.defs[1..].sort_by_key(|d| (d.span.byte_start, d.span.byte_end));
    facts.refs.sort_by_key(|r| r.span.byte_start);
    facts.header.imports.sort_by_key(|i| i.span.byte_start);
    facts
}

/// One `import` declaration and its reference.
///
/// A `#ifdef`-guarded import is an ordinary one here — the pinned grammar puts
/// the guard line beside the declarations as a `cpp` node rather than removing
/// what it guards — **as long as it is in the first arm**. See the module
/// header for the `#else` arm this grammar swallows and what the corpus pays
/// for it.
fn import(facts: &mut FileFacts<HsLang>, node: &SgNode) {
    let Some(name) = node.field("module").and_then(|m| decl_name(&m)) else {
        return; // an `import` the parser could not read a module name out of
    };
    let span = span_of(node);
    facts.header.imports.push(ImportSpec {
        module_name: name.clone(),
        span,
    });
    facts.refs.push(Reference {
        kind: RefKind::Import,
        space: DeclSpace::Namespace,
        // The module name alone. `qualified`, `as` and `hiding` change what
        // this file may write next and not what is named, so two imports of
        // one module from one file are one row carrying a count of two rather
        // than two rows saying the same thing.
        raw_target: name.clone(),
        target: RefTarget {
            root: TargetRoot::Name,
            segments: name.split('.').map(str::to_string).collect(),
        },
        // Tier 2 emits no expression-level reference, so nothing here can name
        // a local: `LocalBinding` does not apply to this track.
        locally_bound: false,
        argc: None,
        arg_types: None,
        // An import sits at the top of a file, inside no declaration, so the
        // driver sources its edge at the file's own module node — which is
        // exactly what an import graph's edges start at.
        enclosing: None,
        span,
    });
}

/// `data T = …` and `newtype T = …`: the type, then everything under it.
fn data_declaration(facts: &mut FileFacts<HsLang>, node: &SgNode) {
    let Some(owner) = placement(node) else { return };
    let Some(name) = node.field("name").and_then(|n| decl_name(&n)) else {
        return;
    };
    facts.defs.push(def(
        DefKind::Type,
        name.clone(),
        owner.clone(),
        DeclSpace::Type,
        DefFacets::default(),
        span_of(node),
    ));
    let mut under = owner;
    under.push(name);
    constructors(facts, node, &under);
}

/// `data instance Sing Bool = SBool`: constructors, and **no** type.
///
/// The name in the head is the family's, declared by the `data family` that
/// opened it; re-declaring it here would be one type counted once per
/// instance. The constructors are new names in the module's value namespace
/// and are filed under the family, which is where a reader looks for them.
fn data_instance(facts: &mut FileFacts<HsLang>, node: &SgNode) {
    let Some(owner) = placement(node) else { return };
    let Some(inner) = node
        .children()
        .find(|c| matches!(&*c.kind(), "data_type" | "newtype"))
    else {
        return;
    };
    let Some(family) = inner.field("name").and_then(|n| decl_name(&n)) else {
        return;
    };
    let mut under = owner;
    under.push(family);
    constructors(facts, &inner, &under);
}

/// Every constructor a `data`/`newtype` head declares, and the record fields
/// each of them names.
fn constructors(facts: &mut FileFacts<HsLang>, node: &SgNode, owner: &[String]) {
    if let Some(list) = node.field("constructors") {
        for child in list.children() {
            match &*child.kind() {
                "data_constructor" => {
                    if let Some(shape) = child.field("constructor") {
                        constructor(facts, &shape, owner);
                    }
                }
                "gadt_constructor" => constructor(facts, &child, owner),
                _ => {}
            }
        }
    }
    // `newtype T = C …` states one constructor in a field of its own.
    if let Some(only) = node.field("constructor") {
        constructor(facts, &only, owner);
    }
}

/// One constructor: every name it declares, then its record fields.
fn constructor(facts: &mut FileFacts<HsLang>, node: &SgNode, owner: &[String]) {
    for named in decl_names(node) {
        let Some(name) = decl_name(&named) else {
            continue;
        };
        facts.defs.push(def(
            DefKind::Constructor,
            name,
            owner.to_vec(),
            DeclSpace::Value,
            DefFacets::default(),
            span_of(&named),
        ));
    }
    for field in field_names(node) {
        let Some(name) = decl_name(&field) else {
            continue;
        };
        // A field selector belongs to the type, not to the constructor: two
        // constructors of one type may declare the same field, and it is one
        // function either way.
        facts.defs.push(def(
            DefKind::Field,
            name,
            owner.to_vec(),
            DeclSpace::Value,
            DefFacets::default(),
            span_of(&field),
        ));
    }
}

/// `type T = …`, `type family T`, `data family T`.
///
/// All three declare a name in the type namespace and nothing in the value
/// one. An associated family inside a class body is filed under the class,
/// which is [`placement`]'s answer and needs no rule of its own here.
fn type_declaration(facts: &mut FileFacts<HsLang>, node: &SgNode) {
    let Some(owner) = placement(node) else { return };
    let Some(name) = node.field("name").and_then(|n| decl_name(&n)) else {
        return;
    };
    facts.defs.push(def(
        DefKind::Type,
        name,
        owner,
        DeclSpace::Type,
        DefFacets::default(),
        span_of(node),
    ));
}

/// `class C a where …`: the class itself. Its members arrive through
/// [`value_declaration`] and [`type_declaration`], which [`placement`] files
/// under it.
fn class_declaration(facts: &mut FileFacts<HsLang>, node: &SgNode) {
    let Some(owner) = placement(node) else { return };
    let Some(name) = node.field("name").and_then(|n| decl_name(&n)) else {
        return;
    };
    facts.defs.push(def(
        DefKind::Type,
        name,
        owner,
        DeclSpace::Type,
        // A class is the one Haskell declaration a `DefFacets` flag already
        // describes: a named set of operations a type may implement.
        DefFacets::INTERFACE,
        span_of(node),
    ));
}

/// A type signature, a function equation, or a plain binding.
///
/// The three are one declaration written in up to three places — `x :: Int`,
/// `x n = …`, `x = …` — and each is emitted. Merging them is the resolver's
/// job, because merging is a statement about identity and this layer makes
/// none.
fn value_declaration(facts: &mut FileFacts<HsLang>, node: &SgNode) {
    let Some(owner) = placement(node) else { return };
    let kind = if owner.is_empty() {
        DefKind::Function
    } else {
        DefKind::Method
    };
    for named in decl_names(node) {
        let Some(name) = decl_name(&named) else {
            continue;
        };
        facts.defs.push(def(
            kind,
            name,
            owner.clone(),
            DeclSpace::Value,
            DefFacets::default(),
            span_of(node),
        ));
    }
}

/// `pattern P :: …` and `pattern P x <- …`: a constructor the module declares.
///
/// A pattern synonym lives in the data-constructor namespace — which is why it
/// cannot share a name with a real constructor in the same module — so it is
/// filed as one, at the top level rather than under a type it has none of.
fn pattern_synonym(facts: &mut FileFacts<HsLang>, node: &SgNode) {
    let Some(owner) = placement(node) else { return };
    for child in node.children() {
        let Some(synonym) = child.field("synonym") else {
            continue;
        };
        // `pattern Head x <- …` writes the synonym applied to its arguments;
        // the head of that application is the name being declared.
        let named = match &*synonym.kind() {
            "apply" => synonym.field("function"),
            "infix" => synonym.field("operator"),
            _ => Some(synonym.clone()),
        };
        let Some(named) = named else { continue };
        let names = if named.kind() == "binding_list" {
            decl_names(&named)
        } else {
            vec![named]
        };
        for one in names {
            let Some(name) = decl_name(&one) else {
                continue;
            };
            facts.defs.push(def(
                DefKind::Constructor,
                name,
                owner.clone(),
                DeclSpace::Value,
                DefFacets::default(),
                span_of(node),
            ));
        }
    }
}

/// `foreign import ccall … name :: …`: the Haskell name the import binds.
///
/// A `foreign export` is deliberately not read: it exports a name the module
/// already declares, so reading it would count one declaration twice.
fn foreign_import(facts: &mut FileFacts<HsLang>, node: &SgNode) {
    let Some(owner) = placement(node) else { return };
    let Some(signature) = node.field("signature") else {
        return;
    };
    for named in decl_names(&signature) {
        let Some(name) = decl_name(&named) else {
            continue;
        };
        facts.defs.push(def(
            DefKind::Function,
            name,
            owner.clone(),
            DeclSpace::Value,
            DefFacets::default(),
            span_of(node),
        ));
    }
}

/// The Haskell extractor, as the driver holds it.
pub struct HsExtractor;

impl Extractor<HsLang> for HsExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<HsLang> {
        extract(rel_path, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_compile() {
        Rules::compile(HASKELL_RULES).expect("haskell.yml compiles");
    }

    #[test]
    fn a_header_declares_the_name_and_its_absence_is_a_different_fact() {
        let with = extract("src/M.hs", "module M where\n");
        assert_eq!(with.header.module_name.as_deref(), Some("M"));
        assert_eq!(with.header.declared_name(), "M");
        let without = extract("examples/src/Simplest.hs", "main = pure ()\n");
        assert_eq!(without.header.module_name, None);
        assert_eq!(without.header.declared_name(), IMPLICIT_MODULE);
    }

    #[test]
    fn an_instance_body_declares_nothing() {
        let facts = extract(
            "src/M.hs",
            "module M where\ninstance ToJSON Bool where\n  toJSON = Bool\n  helper x = x\n",
        );
        assert_eq!(facts.defs.len(), 1, "{:?}", facts.defs);
    }

    #[test]
    fn a_let_bound_name_is_not_a_node() {
        let facts = extract("src/M.hs", "module M where\ngo = let inner = 1 in inner\n");
        let names: Vec<&str> = facts.defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["M", "go"]);
    }

    #[test]
    fn every_import_reference_is_paired_with_a_declaration() {
        // The pairing is by span, so a reference the scope cannot find would
        // silently take the unpaired-reference reason for a perfectly ordinary
        // import. It must be total.
        let facts = extract(
            "src/M.hs",
            "module M where\nimport A\nimport qualified B.C as D\n",
        );
        assert_eq!(facts.refs.len(), 2);
        for r in &facts.refs {
            assert!(
                facts
                    .header
                    .imports
                    .iter()
                    .any(|i| i.span == r.span && i.module_name == r.raw_target),
                "unpaired: {}",
                r.raw_target,
            );
        }
    }
}
