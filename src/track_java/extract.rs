//! Java extractor: one file in, records out. Forbidden from linking.
//!
//! The YAML rule (embedded from `rules/java.yml`) selects nodes by kind; this
//! module interprets them. Case identifiers in the comments — `P-01`, `I-04`,
//! `M-05`, `X-02` and the rest — name numbered cases in the Java case study,
//! which is this track's contract.
//!
//! Three things make Java's extractor bigger than Go's, and all three are
//! *file-local facts* rather than linking:
//!
//! * **A binding environment with extents.** JLS §6.3 scopes a local to the
//!   rest of its block, a parameter to its whole method, a pattern variable
//!   by definite assignment. Whether a name at a site is a local is therefore
//!   a byte-offset question, and [`Binding`] carries the offsets.
//! * **A declared-type environment** (X-02). `Foo f = …; f.m();` states `f`'s
//!   type in the same file. Handing the resolver `{name → declared type}` is
//!   the single largest resolution-rate lever Java has, and it is not type
//!   inference — the type is written down. The extractor states it; the
//!   resolver still owns every outcome.
//! * **Implicit members** (D-10). A record's accessors and canonical
//!   constructor, an enum's `values()`/`valueOf`, a class's default
//!   constructor have no declaration syntax and are named from source anyway.
//!   Synthesizing them from the header removes a whole class of false
//!   `NoMatchingDefinition` that would otherwise read as resolver weakness.
//!
//! What is deliberately *not* here: anonymous and local classes are not
//! definitions and neither are their members (T-03, T-04 — JLS §6.7 gives
//! them no canonical name, and §13.1's `Outer$1` numbering is
//! occurrence-ordered, so using it as a NodeId input would make inserting one
//! anonymous class re-key every later one).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, Params, RefKind, RefTarget, Reference,
    Span, TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_java::JavaLang;
use crate::track_java::fqn;

/// The embedded Java extraction rules.
const JAVA_RULES: &str = include_str!("../rules/java.yml");

/// Which of JLS §7.5's import forms a declaration is.
///
/// The form decides the *tier* a candidate sits at during resolution (N-03),
/// which is why one enum is worth more than a `bool` for `static`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// `import java.util.List;` — §7.5.1. Binds one simple name.
    SingleType,
    /// `import java.util.*;` — §7.5.2. Names a package, not its subpackages.
    TypeOnDemand,
    /// `import static org.junit.Assert.assertEquals;` — §7.5.3. Names a
    /// *member name* on an owner type: an overload set, possibly a field,
    /// possibly a member type, all at once.
    SingleStatic,
    /// `import static java.util.Arrays.*;` — §7.5.4.
    StaticOnDemand,
    /// `import module java.base;` — §7.5.5, JEP 511.
    Module,
}

/// One import declaration: its form and the canonical name it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// Which of the five forms this is.
    pub kind: ImportKind,
    /// The imported canonical name split on `.` — `["java", "util", "Map",
    /// "Entry"]`. The package/type split is *not* decidable here (I-09,
    /// N-04): `java.util.Map` could have been a package, and only the symbol
    /// table knows. Splitting is the resolver's job; segmenting is this
    /// one's.
    pub segments: Vec<String>,
    /// Where the declaration sits.
    pub span: Span,
}

/// What kind of declaration put a name in the binding environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    /// A field of the enclosing type. **A node**: it is in the environment
    /// for its declared type (X-02) and never makes a reference local.
    Field,
    /// A local variable, a `try`-with-resources resource, or an enhanced
    /// `for` variable — JLS §14.4, §14.20.3, §14.14.2.
    Local,
    /// A formal parameter of a method, constructor or lambda — §8.4.1,
    /// §15.27.1.
    Parameter,
    /// An `instanceof` or pattern-match binding — §6.3.1.
    PatternVariable,
    /// A `catch` clause parameter — §14.20.
    CatchParameter,
    /// A type parameter — §4.4. Its scope is the declaration only, so it is
    /// never a node.
    TypeParameter,
    /// A local class name — §14.3. JLS §6.7 gives it no canonical name, so
    /// it is not a node and a type use naming it must not be linked to an
    /// import of the same simple name.
    LocalType,
}

/// The declaration table a bound name shadows a lookup in.
///
/// Java's namespaces are separate (§6.5.1), so this is what stops a local
/// called `list` from making `list()` look local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Namespace {
    /// Variables and fields.
    Value,
    /// Types.
    Type,
}

impl BindingKind {
    /// Which table this binding shadows a name in.
    fn namespace(self) -> Namespace {
        match self {
            BindingKind::Field
            | BindingKind::Local
            | BindingKind::Parameter
            | BindingKind::PatternVariable
            | BindingKind::CatchParameter => Namespace::Value,
            BindingKind::TypeParameter | BindingKind::LocalType => Namespace::Type,
        }
    }

    /// Whether binding a name this way keeps it out of the graph.
    ///
    /// Everything but a field: a field *is* a node (D-05), so `this.count`
    /// and a bare `count` name something the resolver can link, while a local
    /// names something nothing outside its block could ever name.
    pub fn is_local(self) -> bool {
        !matches!(self, BindingKind::Field)
    }
}

/// One name a region of the file binds, and the type its declaration states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The bound simple name.
    pub name: String,
    /// What declared it.
    pub kind: BindingKind,
    /// The declared type as written, split on `.`, or `None` when the
    /// declaration states no usable one: a `var` whose initializer is not
    /// shape-readable (X-03), a lambda's inferred parameter, a type
    /// parameter, a multi-catch, an array or a primitive.
    ///
    /// As *written*: `Entry` and `Map.Entry` and `java.util.Map.Entry` are
    /// three different strings for one type and only the compilation unit's
    /// scope can canonicalize them (N-03). That canonicalization is
    /// resolution, so it is not done here.
    pub declared_type: Option<Vec<String>>,
    /// Byte offset the binding becomes visible at (§6.3).
    pub start: u32,
    /// Byte offset one past the end of the region it is visible in.
    pub end: u32,
}

/// One type declared in this file, with the supertypes it names.
///
/// H-01: member lookup cannot start until `extends`/`implements` have been
/// resolved, and the *names* are a single-file fact. Recording them here is
/// how the resolver reaches an inherited member without an edge table it
/// cannot read — see [`crate::track_java::resolve`] for how far that gets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypeDecl {
    /// Nesting path, outermost first: `["Outer", "Inner"]`.
    pub path: Vec<String>,
    /// The `extends` clause of a class (§8.1.4), as written and split on `.`.
    pub superclass: Option<Vec<String>>,
    /// `implements` on a class (§8.1.5), or `extends` on an interface
    /// (§9.1.3) — both name supertypes whose members are inherited.
    pub interfaces: Vec<Vec<String>>,
}

/// A type declaration the node rule erases: an anonymous class body (T-04),
/// an enum constant's body (T-05), or a local class (T-03).
///
/// None of the three has a canonical name (§6.7), so none is a node, and
/// [`enclosing_definition`] walks straight past it to reach a nameable edge
/// source. That is right for the edge's *source* and wrong for everything
/// else: §15.8.3's `this`, §15.11.2's `super` and §15.12.1's unqualified
/// invocation all search the innermost enclosing type declaration, and these
/// are type declarations. A resolver that reads the edge source back as "the
/// type chain this site sits in" therefore starts one frame too far out and
/// links a member of the anonymous class to a same-named member of the class
/// around it — a wrong edge, not a lowered rate.
///
/// So the frame is recorded rather than erased twice. It is not a node and
/// nothing here makes it one; what it carries is enough for the resolver to
/// know that a name resolved *here* has no nameable target, and enough to
/// reach the supertype the frame actually names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErasedType {
    /// Byte offset the frame's body opens at.
    pub start: u32,
    /// Byte offset one past its close.
    pub end: u32,
    /// The supertype the frame names, as written and split on `.`.
    ///
    /// For `new Base(){…}` this is `Base` (§15.9.5); for an enum constant's
    /// body it is the enum type (§8.9.3); for a local class it is its
    /// `extends` clause. `None` when there is none to name, whose superclass
    /// is then `java.lang.Object` (§8.1.4).
    ///
    /// One caveat stated rather than modelled: `new Iface(){…}` on an
    /// *interface* extends `Object` and only implements `Iface`, and nothing
    /// in one file distinguishes an interface from a class. `super.m()` in
    /// that frame would have to name an `Object` member to compile at all, so
    /// the difference is unreachable in a corpus that compiles.
    pub superclass: Option<Vec<String>>,
    /// Further supertypes whose members it inherits: a local class's
    /// `implements` clause.
    pub interfaces: Vec<Vec<String>>,
    /// The member keys the frame declares itself — [`fqn::member_key`] for a
    /// callable, the bare name for a field.
    ///
    /// A hit here is the whole point: it says the target of this reference is
    /// a member of an unnameable type, so there is no honest edge and the
    /// walk must not continue outward and find a same-named member on a type
    /// the site is not in.
    pub members: BTreeSet<String>,
    /// The simple member names it declares, at any arity.
    ///
    /// §15.12.1 chooses the innermost enclosing type declaration of which the
    /// method is a member *by name*; applicability is decided afterwards. So
    /// the arity-free set is what says "the search stops in this frame".
    pub member_names: BTreeSet<String>,
}

/// Per-file Java facts only the Java resolver reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JavaHeader {
    /// Repo-relative, `/`-separated path of the file.
    pub rel_path: String,
    /// The **declared** package (P-01), dotted, or `None` for the unnamed
    /// package (P-03).
    ///
    /// The directory is metadata and nothing else: JLS §7.2 makes the
    /// package-to-directory mapping implementation-specific, so a file at
    /// `src/main/java/a/b/Foo.java` declaring `package c.d;` is in `c.d`.
    pub package: Option<String>,
    /// The module a `module-info.java` declares (P-05), dotted.
    pub module: Option<String>,
    /// Every import declaration, in source order.
    ///
    /// Each is *also* a [`RefKind::Import`] reference in `refs`: the
    /// reference is the extractor's, the binding effect on the file's scope
    /// is the resolver's.
    pub imports: Vec<Import>,
    /// Every name this file binds, with extents and declared types (X-02).
    pub bindings: Vec<Binding>,
    /// Every type this file declares, in declaration order.
    pub types: Vec<TypeDecl>,
    /// Every anonymous class body, enum-constant body and local class, in
    /// declaration order — the type frames that are not nodes (T-03, T-04,
    /// T-05) and which a reference inside still resolves against.
    pub erased: Vec<ErasedType>,
    /// Every [`crate::track_java::fqn::overload_group`] two or more callables
    /// in this file compete for (M-01).
    ///
    /// A Java type's members are all declared in one compilation unit, so
    /// this is complete for every type in it — which is what lets
    /// [`crate::lang::Resolver::def_fqn`] choose between the arity key and
    /// the signature form without seeing another file.
    pub overloaded: HashSet<String>,
}

impl JavaHeader {
    /// Whether some region enclosing `site` binds `name` in `ns` with a
    /// declaration that is not a node.
    fn binds_locally(&self, name: &str, site: u32, ns: Namespace) -> bool {
        self.bindings.iter().any(|b| {
            b.kind.is_local()
                && b.kind.namespace() == ns
                && b.name == name
                && b.start <= site
                && site < b.end
        })
    }

    /// The innermost value binding visible at `site`.
    fn binding_at(&self, name: &str, site: u32) -> Option<&Binding> {
        self.bindings
            .iter()
            .filter(|b| {
                b.name == name
                    && b.start <= site
                    && site < b.end
                    && matches!(
                        b.kind,
                        BindingKind::Field
                            | BindingKind::Local
                            | BindingKind::Parameter
                            | BindingKind::PatternVariable
                            | BindingKind::CatchParameter
                    )
            })
            .max_by_key(|b| b.start)
    }
}

/// The Java extractor. Stateless.
pub struct JavaExtractor;

impl Extractor<JavaLang> for JavaExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<JavaLang> {
        extract(rel_path, source)
    }
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules::compile(JAVA_RULES).expect("embedded java.yml compiles"))
}

/// A node's text with every ASCII whitespace byte removed.
///
/// Type syntax and qualified names may be written across lines
/// (`Map\n  .Entry`), and `raw_target` is a dedup key before it is display
/// text. Two sites that name the same thing must not land in two rows
/// because one of them wrapped.
fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The dotted segments of a name or type node, ignoring type arguments.
///
/// Type arguments are dropped because erasure does (JLS §4.6, M-02): a
/// reference to `List<String>` and one to `List<Integer>` name one type.
/// Arrays and primitives yield nothing — neither is a nameable definition in
/// this model.
fn name_segments(node: &SgNode) -> Vec<String> {
    match &*node.kind() {
        "identifier" | "type_identifier" => vec![node.text().to_string()],
        "scoped_identifier" => {
            let mut segs = node
                .field("scope")
                .map(|s| name_segments(&s))
                .unwrap_or_default();
            if let Some(name) = node.field("name") {
                segs.push(name.text().to_string());
            }
            segs
        }
        "scoped_type_identifier" => node
            .children()
            .filter(|c| c.is_named() && c.kind() != "type_arguments" && !is_annotation(c))
            .flat_map(|c| name_segments(&c))
            .collect(),
        "generic_type" | "annotated_type" => node
            .children()
            .find(|c| c.is_named() && c.kind() != "type_arguments" && !is_annotation(c))
            .map(|c| name_segments(&c))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn is_annotation(node: &SgNode) -> bool {
    matches!(&*node.kind(), "annotation" | "marker_annotation")
}

/// `Some` when the segments name something; `None` when they name nothing a
/// definition could be — a primitive, an array, an unreadable `var`.
fn some_type(segments: Vec<String>) -> Option<Vec<String>> {
    (!segments.is_empty()).then_some(segments)
}

/// A partially parsed qualifier chain: what the leftmost thing is, plus the
/// dotted selectors after it.
struct Chain {
    root: TargetRoot,
    segments: Vec<String>,
}

fn expr_chain() -> Chain {
    Chain {
        root: TargetRoot::Expr,
        segments: Vec::new(),
    }
}

/// Walk a receiver expression into a [`Chain`].
///
/// `a.b.c` is a name chain; `Outer.this` and `Iface.super` are *roots* with a
/// qualifier rather than member selections (§15.8.4, §15.12.1); anything
/// whose leftmost thing is not a name — a call, an index, a cast — is
/// [`TargetRoot::Expr`] and honestly needs typing (X-01).
fn access_chain(node: &SgNode) -> Chain {
    match &*node.kind() {
        "identifier" => Chain {
            root: TargetRoot::Name,
            segments: vec![node.text().to_string()],
        },
        "this" => Chain {
            root: TargetRoot::This {
                qualifier: Vec::new(),
            },
            segments: Vec::new(),
        },
        "super" => Chain {
            root: TargetRoot::Super {
                qualifier: Vec::new(),
            },
            segments: Vec::new(),
        },
        "field_access" => {
            let Some(field) = node.field("field") else {
                return expr_chain();
            };
            let base = node
                .field("object")
                .map(|o| access_chain(&o))
                .unwrap_or_else(expr_chain);
            match &*field.kind() {
                "this" | "super" => {
                    if !matches!(base.root, TargetRoot::Name) {
                        return expr_chain();
                    }
                    let qualifier = base.segments;
                    Chain {
                        root: if field.kind() == "this" {
                            TargetRoot::This { qualifier }
                        } else {
                            TargetRoot::Super { qualifier }
                        },
                        segments: Vec::new(),
                    }
                }
                _ => {
                    let mut chain = base;
                    chain.segments.push(field.text().to_string());
                    chain
                }
            }
        }
        _ => expr_chain(),
    }
}

/// The target shape of a `method_invocation`.
fn call_target(inv: &SgNode) -> RefTarget {
    let name = inv
        .field("name")
        .map(|n| n.text().to_string())
        .unwrap_or_default();
    let object = inv.field("object");
    // `Iface.super.m()`: the grammar leaves `Iface` in `object` and the
    // `super` token as a plain child, so the qualified-superinterface form is
    // only visible as an extra child.
    let qualified_super = inv
        .children()
        .any(|c| c.kind() == "super" && object.as_ref().is_none_or(|o| o.range() != c.range()));
    let mut chain = match &object {
        Some(o) => access_chain(o),
        None => Chain {
            root: TargetRoot::Name,
            segments: Vec::new(),
        },
    };
    if qualified_super {
        let qualifier = if matches!(chain.root, TargetRoot::Name) {
            std::mem::take(&mut chain.segments)
        } else {
            Vec::new()
        };
        chain.root = TargetRoot::Super { qualifier };
        chain.segments.clear();
    }
    chain.segments.push(name);
    RefTarget {
        root: chain.root,
        segments: chain.segments,
    }
}

/// The number of arguments at a call or creation site (M-06).
///
/// The one fact a Java call site has about the callee's signature that the
/// callee's *name* does not carry, and therefore the minimum for any overload
/// discrimination at all (M-04).
fn argument_count(node: &SgNode) -> Option<u32> {
    let list = node.field("arguments")?;
    let count = list
        .children()
        .filter(|c| c.is_named() && c.kind() != "comment")
        .count();
    u32::try_from(count).ok()
}

/// A source-level type that is evident from an argument expression alone.
///
/// This is deliberately a cut line, not a miniature type checker: literals,
/// declared names, casts and class creation are readable in one file.
/// Calls, general operators, conditionals, lambdas, method references, arrays,
/// `null`, and anything else stay unknown and therefore cannot narrow an
/// overload set. Unary `+`, `-`, and `~` over a numeric literal are included:
/// unary numeric promotion preserves the literal's `int`/`long` type here.
fn argument_type(node: &SgNode, header: &JavaHeader, site: u32) -> Option<String> {
    match &*node.kind() {
        "string_literal" => Some("String".to_string()),
        "character_literal" => Some("char".to_string()),
        "true" | "false" => Some("boolean".to_string()),
        "decimal_integer_literal"
        | "hex_integer_literal"
        | "octal_integer_literal"
        | "binary_integer_literal" => {
            let text = node.text();
            Some(
                if text.ends_with('l') || text.ends_with('L') {
                    "long"
                } else {
                    "int"
                }
                .to_string(),
            )
        }
        "decimal_floating_point_literal" | "hex_floating_point_literal" => {
            let text = node.text();
            Some(
                if text.ends_with('f') || text.ends_with('F') {
                    "float"
                } else {
                    "double"
                }
                .to_string(),
            )
        }
        "identifier" => header
            .binding_at(&node.text(), site)
            .and_then(|binding| binding.declared_type.as_ref())
            .map(|segments| segments.join(".")),
        "cast_expression" | "object_creation_expression" => node
            .field("type")
            .map(|ty| compact(&ty.text()))
            .filter(|ty| !ty.is_empty()),
        "parenthesized_expression" => node
            .children()
            .find(|child| child.is_named())
            .and_then(|child| argument_type(&child, header, site)),
        "unary_expression" if node.text().trim_start().starts_with(['+', '-', '~']) => {
            let promoted = node
                .children()
                .find(|child| child.is_named())
                .and_then(|child| argument_type(&child, header, site))?;
            matches!(
                promoted.as_str(),
                "byte" | "short" | "char" | "int" | "long" | "float" | "double"
            )
            .then_some(promoted)
        }
        _ => None,
    }
}

/// The complete argument-type vector when every argument is file-local.
fn argument_types(node: &SgNode, header: &JavaHeader) -> Option<Vec<String>> {
    let site = node.range().start as u32;
    node.field("arguments").and_then(|list| {
        list.children()
            .filter(|child| child.is_named() && child.kind() != "comment")
            .map(|child| argument_type(&child, header, site))
            .collect::<Option<Vec<_>>>()
    })
}

/// The literal text of a call site's callee, minus explicit type arguments.
///
/// M-08: `Collections.<String>emptyList()` and `Collections.emptyList()` name
/// the same method — the type arguments constrain inference, not identity —
/// so they must not become two dedup rows.
fn call_raw_target(source: &str, inv: &SgNode, name_end: usize) -> String {
    let start = inv.range().start;
    let type_args = inv.field("type_arguments").map(|t| t.range());
    let mut out = String::new();
    for (offset, ch) in source[start..name_end].char_indices() {
        let at = start + offset;
        if type_args.as_ref().is_some_and(|r| r.contains(&at)) || ch.is_whitespace() {
            continue;
        }
        out.push(ch);
    }
    out
}

/// Whether a type declaration is *local* — declared in a statement position.
///
/// JLS §6.7: a local class has no canonical name, so it is not a node and
/// neither are its members. That is the spec stating this project's node rule
/// rather than a judgement call.
fn is_local_type(node: &SgNode) -> bool {
    node.parent().is_some_and(|p| {
        matches!(
            &*p.kind(),
            "block" | "constructor_body" | "switch_block" | "switch_block_statement_group"
        )
    })
}

fn is_type_declaration(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    )
}

/// Whether a `class_body` is an anonymous class's or an enum constant's.
///
/// Both declare a class with no canonical name (§15.9.5, §8.9.3), so nothing
/// inside either is nameable.
fn is_anonymous_body(node: &SgNode) -> bool {
    node.kind() == "class_body"
        && node
            .parent()
            .is_some_and(|p| matches!(&*p.kind(), "object_creation_expression" | "enum_constant"))
}

/// The enclosing nameable type chain, outermost first, or `None` when some
/// enclosing type has no canonical name.
///
/// `None` is the whole of "this is not a definition": a method of an
/// anonymous class and a field of a local class both return it.
fn owner_chain(node: &SgNode) -> Option<Vec<String>> {
    let mut chain: Vec<String> = Vec::new();
    for ancestor in node.ancestors() {
        if is_anonymous_body(&ancestor) {
            return None;
        }
        if is_type_declaration(&ancestor.kind()) {
            if is_local_type(&ancestor) {
                return None;
            }
            chain.push(ancestor.field("name")?.text().to_string());
        }
    }
    chain.reverse();
    Some(chain)
}

/// A member's path segment: its name, plus its source-level parameter list
/// when it has one.
///
/// **A core-shape workaround, stated plainly.** [`Encloser`] carries
/// `path: Vec<String>` and no parameter shape, and Java method identity is
/// name *plus signature* (§8.4.2). Two overloads of one method are two nodes
/// (M-01), so an edge sourced at "the enclosing method" must be able to say
/// *which* overload — otherwise every call inside `m(String)` and every call
/// inside `m(int)` start at the same node, which is silent graph corruption
/// rather than a lowered rate. Carrying the source-level parameter list in
/// the last path segment is the smallest way to say it without touching the
/// core; the resolver reads it back when it builds the FQN.
fn member_segment(node: &SgNode) -> String {
    let name = node
        .field("name")
        .map(|n| n.text().to_string())
        .unwrap_or_default();
    match parameters_of(node) {
        Some(params) => format!("{name}({})", params.types.join(",")),
        None => name,
    }
}

/// The nearest *nameable* enclosing definition of a reference site.
///
/// Lambdas are transparent (T-06) and so are anonymous and local classes
/// (T-03, T-04): a call inside one belongs to the nameable member around it,
/// because that is the only node an edge could start at. A call in a field
/// initializer or a static initializer belongs to the owning **type** (D-11)
/// — a type is a node and JVMS §2.9.2's `<clinit>` is not a nameable name.
fn enclosing_definition(node: &SgNode) -> Option<Encloser> {
    let mut member: Option<(String, DefKind)> = None;
    let mut types: Vec<String> = Vec::new();
    for ancestor in node.ancestors() {
        let kind = ancestor.kind();
        if is_anonymous_body(&ancestor) {
            member = None;
            types.clear();
            continue;
        }
        match &*kind {
            "method_declaration" | "annotation_type_element_declaration" if member.is_none() => {
                member = Some((member_segment(&ancestor), DefKind::Method));
            }
            "constructor_declaration" | "compact_constructor_declaration" if member.is_none() => {
                member = Some((member_segment(&ancestor), DefKind::Constructor));
            }
            k if is_type_declaration(k) => {
                if is_local_type(&ancestor) {
                    member = None;
                    types.clear();
                } else if let Some(name) = ancestor.field("name") {
                    types.push(name.text().to_string());
                }
            }
            _ => {}
        }
    }
    types.reverse();
    match member {
        Some((segment, kind)) => {
            let mut path = types;
            path.push(segment);
            Some(Encloser { path, kind })
        }
        None if !types.is_empty() => Some(Encloser {
            path: types,
            kind: DefKind::Type,
        }),
        None => None,
    }
}

/// The parameter shape of a declaration that has one (M-05).
///
/// A compact constructor (§8.10.4) has no parameter list of its own: its
/// parameters *are* the record's components, so they are read from the
/// record header.
fn parameters_of(node: &SgNode) -> Option<Params> {
    match &*node.kind() {
        "method_declaration" | "constructor_declaration" | "lambda_expression" => {
            Some(params_from_list(&node.field("parameters")?))
        }
        "annotation_type_element_declaration" => Some(Params {
            count: 0,
            varargs: false,
            types: Vec::new(),
        }),
        "compact_constructor_declaration" => {
            let record = node
                .ancestors()
                .find(|a| a.kind() == "record_declaration")?;
            Some(params_from_list(&record.field("parameters")?))
        }
        _ => None,
    }
}

/// Parameter types as written, in order. A variable-arity parameter keeps its
/// `...` so the string round-trips: §8.4.1 makes its declared type an array
/// type, and dropping the marker would make `f(int)` and `f(int...)` the same
/// key while §15.12.2 says a fixed-arity method always beats a varargs one.
fn params_from_list(list: &SgNode) -> Params {
    let mut types: Vec<String> = Vec::new();
    let mut varargs = false;
    for param in list.children() {
        match &*param.kind() {
            "formal_parameter" => {
                types.push(
                    param
                        .field("type")
                        .map(|t| compact(&t.text()))
                        .unwrap_or_default(),
                );
            }
            "spread_parameter" => {
                varargs = true;
                let declared = param
                    .children()
                    .find(|c| {
                        c.is_named()
                            && !matches!(&*c.kind(), "modifiers" | "variable_declarator")
                            && !is_annotation(c)
                    })
                    .map(|t| compact(&t.text()))
                    .unwrap_or_default();
                types.push(format!("{declared}..."));
            }
            _ => {}
        }
    }
    Params {
        count: u32::try_from(types.len()).unwrap_or(u32::MAX),
        varargs,
        types,
    }
}

/// Facets from a declaration's written modifiers (M-10).
///
/// **A core-shape gap, narrowed.** JLS §6.6.1 has four access levels and
/// [`DefFacets`] has two bits that speak about them: `public` is
/// [`DefFacets::EXPORTED`] and `private` is [`DefFacets::PRIVATE`], leaving
/// `protected` and package-private indistinguishable from each other. Those
/// two are the ones a subtype *does* inherit (§8.2), so the gap costs a
/// candidate set that is never too small — the resolver cannot filter a
/// package-private member out of another package's list — and the failure
/// mode stays `AmbiguousOverload`, which is honest, rather than a wrong edge.
///
/// `private` is the level that had to be separated, because it is the one
/// that changes what a *closure* contains: §8.2 does not inherit a private
/// member into a subclass at all, so a walk that returned one produced an
/// edge to a body the subclass cannot name.
fn modifier_facets(node: &SgNode) -> DefFacets {
    let mut facets = DefFacets::RUNTIME;
    let Some(modifiers) = node.children().find(|c| c.kind() == "modifiers") else {
        return facets;
    };
    for token in modifiers.children() {
        facets = match &*token.kind() {
            "public" => facets.union(DefFacets::EXPORTED),
            "private" => facets.union(DefFacets::PRIVATE),
            "static" => facets.union(DefFacets::STATIC),
            "abstract" => facets.union(DefFacets::ABSTRACT),
            _ => facets,
        };
    }
    facets
}

/// Modifiers a member has without writing them: every interface and
/// annotation-type member is implicitly public (§9.3, §9.4, §9.6.1), and an
/// interface field is also implicitly static (§9.3).
fn implicit_member_facets(node: &SgNode) -> DefFacets {
    let inside_interface = node
        .parent()
        .is_some_and(|p| matches!(&*p.kind(), "interface_body" | "annotation_type_body"));
    if !inside_interface {
        return DefFacets::default();
    }
    let facets = DefFacets::EXPORTED;
    match &*node.kind() {
        "constant_declaration" | "field_declaration" => facets.union(DefFacets::STATIC),
        _ => facets,
    }
}

/// Facets that come from the kind of type a declaration is.
fn type_kind_facets(kind: &str) -> DefFacets {
    match kind {
        // An interface is implicitly abstract (§9.1.1.1).
        "interface_declaration" => DefFacets::INTERFACE.union(DefFacets::ABSTRACT),
        // An annotation type *is* an interface (§9.6).
        "annotation_type_declaration" => DefFacets::ANNOTATION
            .union(DefFacets::INTERFACE)
            .union(DefFacets::ABSTRACT),
        "enum_declaration" => DefFacets::ENUM,
        "record_declaration" => DefFacets::RECORD,
        _ => DefFacets::default(),
    }
}

/// The member declarations of a type body, flattening an enum's
/// `enum_body_declarations` wrapper.
fn body_members<'r>(body: &SgNode<'r>) -> Vec<SgNode<'r>> {
    let mut out = Vec::new();
    for child in body.children() {
        if child.kind() == "enum_body_declarations" {
            out.extend(child.children().filter(|n| n.is_named()));
        } else if child.is_named() {
            out.push(child);
        }
    }
    out
}

/// Whether a type body declares a method of this name and arity.
fn declares_method(body: &SgNode, name: &str, argc: usize) -> bool {
    body_members(body).iter().any(|m| {
        m.kind() == "method_declaration"
            && m.field("name").is_some_and(|n| n.text() == name)
            && parameters_of(m).is_some_and(|p| p.count as usize == argc)
    })
}

/// Whether a type body declares any constructor. §8.8.9 makes the default
/// constructor conditional on exactly this.
fn declares_constructor(body: &SgNode) -> bool {
    body_members(body).iter().any(|m| {
        matches!(
            &*m.kind(),
            "constructor_declaration" | "compact_constructor_declaration"
        )
    })
}

/// Whether a type body declares a constructor of this arity.
fn declares_constructor_arity(body: &SgNode, argc: usize) -> bool {
    body_members(body).iter().any(|m| {
        matches!(
            &*m.kind(),
            "constructor_declaration" | "compact_constructor_declaration"
        ) && parameters_of(m).is_some_and(|p| p.count as usize == argc)
    })
}

/// A definition record with the fields Java always fills in the same way.
fn java_def(
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

/// The span a synthesized member is attributed to: the declaration header
/// that implies it. A record's accessor really is written down — as the
/// component — and pointing at it is the honest answer to "where is this".
fn synthetic(
    kind: DefKind,
    name: &str,
    owner: &[String],
    facets: DefFacets,
    types: Vec<String>,
    span: Span,
) -> Definition {
    let count = u32::try_from(types.len()).unwrap_or(u32::MAX);
    java_def(
        kind,
        name.to_string(),
        owner.to_vec(),
        DeclSpace::Value,
        facets.union(DefFacets::SYNTHETIC).union(DefFacets::RUNTIME),
        Some(Params {
            count,
            varargs: false,
            types,
        }),
        span,
    )
}

/// A node's byte extent as `(start, end)`.
fn extent(node: &SgNode) -> (u32, u32) {
    let range = node.range();
    (range.start as u32, range.end as u32)
}

/// Record one bound name, unless it is the unnamed variable.
///
/// N-07 / JEP 456: `_` declares nothing nameable, so it binds nothing and a
/// second `_` in the same block is not a redeclaration.
fn push_binding(
    out: &mut Vec<Binding>,
    name: String,
    kind: BindingKind,
    declared_type: Option<Vec<String>>,
    start: u32,
    end: u32,
) {
    if name == "_" {
        return;
    }
    out.push(Binding {
        name,
        kind,
        declared_type,
        start,
        end,
    });
}

/// The declared type a declaration states, applying the `var` shape rule.
///
/// X-03: a `var` local's type is statically evident *from this file* when the
/// initializer is a class instance creation (§15.9) or a cast (§15.16).
/// `var x = f();` needs `f`'s return type — one level of chaining, i.e.
/// inference — so it states nothing and the resolver falls through honestly.
fn declared_type_from(type_node: Option<SgNode>, value: Option<SgNode>) -> Option<Vec<String>> {
    let declared = type_node?;
    if declared.kind() == "type_identifier" && declared.text() == "var" {
        let initializer = value?;
        if !matches!(
            &*initializer.kind(),
            "object_creation_expression" | "cast_expression"
        ) {
            return None;
        }
        return some_type(name_segments(&initializer.field("type")?));
    }
    some_type(name_segments(&declared))
}

/// The end of the region a statement-scoped binding is visible in.
fn enclosing_statement_end(node: &SgNode) -> u32 {
    node.ancestors()
        .find(|a| a.kind().ends_with("_statement") || a.kind() == "local_variable_declaration")
        .or_else(|| node.parent())
        .map_or_else(|| node.range().end as u32, |a| a.range().end as u32)
}

/// The end of the region a `switch` pattern binding is visible in.
fn enclosing_case_end(node: &SgNode) -> u32 {
    node.ancestors()
        .find(|a| matches!(&*a.kind(), "switch_block_statement_group" | "switch_rule"))
        .map_or_else(|| enclosing_statement_end(node), |a| a.range().end as u32)
}

/// Everything one node contributes to the file's binding environment.
///
/// Extents, not a boolean: JLS §6.3 starts a local's scope at its own
/// declaration and ends it at the enclosing block, so `x()` before `Foo x` and
/// `x()` after it are different questions about the same name. Answering with
/// presence alone would move real references into the local bucket, which
/// raises the resolution rate by deleting them from *both* of its terms.
fn collect_bindings(node: &SgNode, out: &mut Vec<Binding>) {
    match &*node.kind() {
        "formal_parameter" => {
            let Some(name) = node.field("name") else {
                return;
            };
            let declared = declared_type_from(node.field("type"), None);
            let Some(owner) = node.parent().and_then(|p| p.parent()) else {
                return;
            };
            let (start, end) = extent(&owner);
            // A record component is a field and an accessor (D-09), not a
            // local: its declared type belongs in the environment, and its
            // name must never make a reference local.
            let kind = if owner.kind() == "record_declaration" {
                BindingKind::Field
            } else {
                BindingKind::Parameter
            };
            push_binding(out, name.text().to_string(), kind, declared, start, end);
        }
        "spread_parameter" => {
            let Some(declarator) = node.children().find(|c| c.kind() == "variable_declarator")
            else {
                return;
            };
            let Some(name) = declarator.field("name") else {
                return;
            };
            let Some(owner) = node.parent().and_then(|p| p.parent()) else {
                return;
            };
            let (start, end) = extent(&owner);
            // §8.4.1 makes a variable-arity parameter's declared type an
            // array type, and this model holds no array members (D-10 leaves
            // `length` and `clone` out), so it states no usable type.
            push_binding(
                out,
                name.text().to_string(),
                BindingKind::Parameter,
                None,
                start,
                end,
            );
        }
        "lambda_expression" => {
            // §15.27.1. A `formal_parameters` list is read by the branch
            // above; only the inferred forms are this node's to bind.
            let Some(parameters) = node.field("parameters") else {
                return;
            };
            let (start, end) = extent(node);
            let mut bind = |n: &SgNode| {
                push_binding(
                    out,
                    n.text().to_string(),
                    BindingKind::Parameter,
                    None,
                    start,
                    end,
                );
            };
            match &*parameters.kind() {
                "identifier" => bind(&parameters),
                "inferred_parameters" => {
                    for id in parameters.children().filter(|c| c.kind() == "identifier") {
                        bind(&id);
                    }
                }
                _ => {}
            }
        }
        "local_variable_declaration" => {
            let type_node = node.field("type");
            let end = node
                .parent()
                .map_or_else(|| node.range().end as u32, |p| p.range().end as u32);
            for declarator in node.field_children("declarator") {
                let Some(name) = declarator.field("name") else {
                    continue;
                };
                let declared = declared_type_from(type_node.clone(), declarator.field("value"));
                push_binding(
                    out,
                    name.text().to_string(),
                    BindingKind::Local,
                    declared,
                    declarator.range().start as u32,
                    end,
                );
            }
        }
        "catch_formal_parameter" => {
            let Some(name) = node.field("name") else {
                return;
            };
            // A multi-catch parameter's type is the least upper bound of the
            // alternatives (§14.20), which is not a name written anywhere, so
            // only a single alternative states a type.
            let alternatives: Vec<SgNode> = node
                .children()
                .filter(|c| c.kind() == "catch_type")
                .flat_map(|t| t.children().filter(|c| c.is_named()).collect::<Vec<_>>())
                .collect();
            let declared = match alternatives.as_slice() {
                [only] => some_type(name_segments(only)),
                _ => None,
            };
            let (start, end) = node
                .parent()
                .map_or_else(|| extent(node), |clause| extent(&clause));
            push_binding(
                out,
                name.text().to_string(),
                BindingKind::CatchParameter,
                declared,
                start,
                end,
            );
        }
        "resource" => {
            let Some(name) = node.field("name") else {
                return; // a bare expression resource declares nothing
            };
            let declared = declared_type_from(node.field("type"), node.field("value"));
            let end = node
                .ancestors()
                .find(|a| a.kind() == "try_with_resources_statement")
                .map_or_else(|| node.range().end as u32, |t| t.range().end as u32);
            push_binding(
                out,
                name.text().to_string(),
                BindingKind::Local,
                declared,
                node.range().start as u32,
                end,
            );
        }
        "enhanced_for_statement" => {
            let Some(name) = node.field("name") else {
                return;
            };
            let declared = declared_type_from(node.field("type"), None);
            push_binding(
                out,
                name.text().to_string(),
                BindingKind::Local,
                declared,
                name.range().start as u32,
                node.range().end as u32,
            );
        }
        "instanceof_expression" => {
            let Some(name) = node.field("name") else {
                return; // no pattern, so no binding
            };
            let declared = node
                .field("right")
                .and_then(|r| some_type(name_segments(&r)));
            // N-06: §6.3.1 scopes a pattern variable by definite assignment
            // rather than by block nesting. Binding it for the whole enclosing
            // statement is deliberately conservative and safe for a *declared
            // type*, because §6.4.1 forbids a conflicting declaration in the
            // same region.
            push_binding(
                out,
                name.text().to_string(),
                BindingKind::PatternVariable,
                declared,
                name.range().start as u32,
                enclosing_statement_end(node),
            );
        }
        "type_pattern" => {
            let named: Vec<SgNode> = node.children().filter(|c| c.is_named()).collect();
            let [type_node, name] = named.as_slice() else {
                return; // a record pattern nests further; its own leaves bind
            };
            push_binding(
                out,
                name.text().to_string(),
                BindingKind::PatternVariable,
                some_type(name_segments(type_node)),
                name.range().start as u32,
                enclosing_case_end(node),
            );
        }
        "type_parameter" => {
            let Some(name) = node.children().find(|c| c.kind() == "type_identifier") else {
                return;
            };
            // §4.4: a type parameter's scope is its declaration, and it is
            // never a node — so `T` naming it must not link to a class `T`.
            let Some(owner) = node.parent().and_then(|p| p.parent()) else {
                return;
            };
            let (start, end) = extent(&owner);
            // X-07: the *bound* is what a receiver of this type is looked up
            // on (§4.4), and it is written right here. An unbounded parameter
            // records `None` and erases to `Object` (§4.6) at resolution.
            let bound = node
                .children()
                .find(|c| c.kind() == "type_bound")
                .and_then(|b| {
                    b.children()
                        .filter(|c| c.is_named())
                        .find_map(|c| head_of(&c))
                })
                .map(|head| name_segments(&head))
                .filter(|segments| !segments.is_empty());
            push_binding(
                out,
                name.text().to_string(),
                BindingKind::TypeParameter,
                bound,
                start,
                end,
            );
        }
        "field_declaration" | "constant_declaration" => {
            let type_node = node.field("type");
            // §8.3.3: a field is in scope throughout its class body,
            // whichever order the members are written in.
            let (start, end) = node
                .parent()
                .map_or_else(|| extent(node), |body| extent(&body));
            for declarator in node.field_children("declarator") {
                let Some(name) = declarator.field("name") else {
                    continue;
                };
                push_binding(
                    out,
                    name.text().to_string(),
                    BindingKind::Field,
                    declared_type_from(type_node.clone(), None),
                    start,
                    end,
                );
            }
        }
        "enum_constant" => {
            let Some(name) = node.field("name") else {
                return;
            };
            let declared = node
                .ancestors()
                .find(|a| a.kind() == "enum_declaration")
                .and_then(|e| e.field("name"))
                .map(|n| vec![n.text().to_string()]);
            let (start, end) = node
                .parent()
                .map_or_else(|| extent(node), |body| extent(&body));
            push_binding(
                out,
                name.text().to_string(),
                BindingKind::Field,
                declared,
                start,
                end,
            );
        }
        kind if is_type_declaration(kind) && is_local_type(node) => {
            let Some(name) = node.field("name") else {
                return;
            };
            let end = node
                .parent()
                .map_or_else(|| node.range().end as u32, |p| p.range().end as u32);
            push_binding(
                out,
                name.text().to_string(),
                BindingKind::LocalType,
                None,
                node.range().start as u32,
                end,
            );
        }
        _ => {}
    }
}

/// The head type node of a supertype entry — the part that names the type,
/// with type arguments and annotations peeled off.
fn head_of<'r>(node: &SgNode<'r>) -> Option<SgNode<'r>> {
    match &*node.kind() {
        "type_identifier" | "scoped_type_identifier" => Some(node.clone()),
        "generic_type" | "annotated_type" => node
            .children()
            .find(|c| c.is_named() && c.kind() != "type_arguments" && !is_annotation(c))
            .and_then(|c| head_of(&c)),
        _ => None,
    }
}

/// Which clause named a supertype. §8.4.8's asymmetry — a class does not
/// inherit `static` methods from a superinterface — and `super.m()`'s meaning
/// both turn on the difference, so the two are kept apart rather than merged
/// into one list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuperRole {
    /// A class's `extends` (§8.1.4): the single superclass.
    Superclass,
    /// `implements` (§8.1.5), or an interface's `extends` (§9.1.3).
    Interface,
}

/// The head nodes of a type declaration's `extends` and `implements` clauses,
/// each with the clause that named it.
///
/// H-01: these are the references member lookup cannot start without, so they
/// are [`RefKind::Inherit`] rather than plain type uses. `permits` is
/// deliberately absent — it names *subtypes*, which is a type use and not a
/// supertype.
fn supertype_heads<'r>(node: &SgNode<'r>) -> Vec<(SuperRole, SgNode<'r>)> {
    let mut heads = Vec::new();
    let clauses = node.children().filter(|c| {
        matches!(
            &*c.kind(),
            "superclass" | "super_interfaces" | "extends_interfaces"
        )
    });
    for clause in clauses {
        let role = if clause.kind() == "superclass" {
            SuperRole::Superclass
        } else {
            SuperRole::Interface
        };
        for child in clause.children().filter(|c| c.is_named()) {
            let entries: Vec<SgNode> = if child.kind() == "type_list" {
                child.children().filter(|c| c.is_named()).collect()
            } else {
                vec![child]
            };
            heads.extend(entries.iter().filter_map(head_of).map(|h| (role, h)));
        }
    }
    heads
}

/// The members a declaration implies but does not write (D-10).
///
/// An extractor that only emits definitions written in declaration syntax
/// produces a false `NoMatchingDefinition` for every `new Point(1, 2)`, every
/// `p.x()` on a record, and every `Color.values()` — misread as resolver
/// weakness when it is a missing row. Synthesis is single-file, mechanical,
/// and flagged [`DefFacets::SYNTHETIC`].
fn synthesize_members(node: &SgNode, owner: &[String], defs: &mut Vec<Definition>) {
    let header_span = node
        .field("name")
        .map_or_else(|| span_of(node), |n| span_of(&n));
    let public = modifier_facets(node);
    let exported = if public.contains(DefFacets::EXPORTED) {
        DefFacets::EXPORTED
    } else {
        DefFacets::default()
    };
    let body = node.field("body");
    match &*node.kind() {
        "record_declaration" => {
            let Some(body) = body else { return };
            let components: Vec<SgNode> = node
                .field("parameters")
                .map(|p| {
                    p.children()
                        .filter(|c| c.kind() == "formal_parameter")
                        .collect()
                })
                .unwrap_or_default();
            // §8.10.3: one private final field and one public accessor per
            // component.
            for component in &components {
                let Some(name) = component.field("name") else {
                    continue;
                };
                let name = name.text().to_string();
                defs.push(java_def(
                    DefKind::Field,
                    name.clone(),
                    owner.to_vec(),
                    DeclSpace::Value,
                    DefFacets::SYNTHETIC.union(DefFacets::RUNTIME),
                    None,
                    span_of(component),
                ));
                if !declares_method(&body, &name, 0) {
                    defs.push(synthetic(
                        DefKind::Method,
                        &name,
                        owner,
                        DefFacets::EXPORTED,
                        Vec::new(),
                        span_of(component),
                    ));
                }
            }
            // §8.10.4: the canonical constructor, unless one is written —
            // compact or explicit, both of which have the component arity.
            let component_types: Vec<String> = node
                .field("parameters")
                .map(|p| params_from_list(&p).types)
                .unwrap_or_default();
            if !declares_constructor_arity(&body, component_types.len()) {
                let name = owner.last().cloned().unwrap_or_default();
                defs.push(synthetic(
                    DefKind::Constructor,
                    &name,
                    owner,
                    exported,
                    component_types,
                    header_span,
                ));
            }
            // §8.10.2: `equals`, `hashCode` and `toString` are implicitly
            // declared by the record itself, not merely inherited.
            for (name, params) in [
                ("equals", vec!["Object".to_string()]),
                ("hashCode", Vec::new()),
                ("toString", Vec::new()),
            ] {
                if !declares_method(&body, name, params.len()) {
                    defs.push(synthetic(
                        DefKind::Method,
                        name,
                        owner,
                        DefFacets::EXPORTED,
                        params,
                        header_span,
                    ));
                }
            }
        }
        "enum_declaration" => {
            let Some(body) = body else { return };
            // §8.9.3: `values()` and `valueOf(String)` are implicitly declared
            // public static members of every enum.
            if !declares_method(&body, "values", 0) {
                defs.push(synthetic(
                    DefKind::Method,
                    "values",
                    owner,
                    DefFacets::EXPORTED.union(DefFacets::STATIC),
                    Vec::new(),
                    header_span,
                ));
            }
            if !declares_method(&body, "valueOf", 1) {
                defs.push(synthetic(
                    DefKind::Method,
                    "valueOf",
                    owner,
                    DefFacets::EXPORTED.union(DefFacets::STATIC),
                    vec!["String".to_string()],
                    header_span,
                ));
            }
            if !declares_constructor(&body) {
                let name = owner.last().cloned().unwrap_or_default();
                defs.push(synthetic(
                    DefKind::Constructor,
                    &name,
                    owner,
                    DefFacets::default(),
                    Vec::new(),
                    header_span,
                ));
            }
        }
        "class_declaration" => {
            // §8.8.9: a class with no constructor declaration implicitly
            // declares a no-argument one with the class's own access.
            let Some(body) = body else { return };
            if !declares_constructor(&body) {
                let name = owner.last().cloned().unwrap_or_default();
                defs.push(synthetic(
                    DefKind::Constructor,
                    &name,
                    owner,
                    exported,
                    Vec::new(),
                    header_span,
                ));
            }
        }
        _ => {}
    }
}

/// The member keys and simple names a class body declares directly.
///
/// Direct members only: a type nested inside the frame is its own scope and
/// its members are not members of the frame.
fn frame_members(body: &SgNode) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut keys = BTreeSet::new();
    let mut names = BTreeSet::new();
    for member in body.children().filter(|c| c.is_named()) {
        match &*member.kind() {
            "method_declaration" | "annotation_type_element_declaration" => {
                let Some(name) = member.field("name") else {
                    continue;
                };
                let name = name.text().to_string();
                let params = parameters_of(&member).unwrap_or(Params {
                    count: 0,
                    types: Vec::new(),
                    varargs: false,
                });
                keys.insert(fqn::member_key(&name, params.count, params.varargs));
                names.insert(name);
            }
            "field_declaration" | "constant_declaration" => {
                for declarator in member.field_children("declarator") {
                    let Some(name) = declarator.field("name") else {
                        continue;
                    };
                    let name = name.text().to_string();
                    keys.insert(name.clone());
                    names.insert(name);
                }
            }
            _ => {}
        }
    }
    (keys, names)
}

/// A declaration's `class_body`, however the grammar attaches it.
///
/// `class_declaration` and `enum_constant` name it with a `body` field;
/// `object_creation_expression` does not name it at all, because its optional
/// class body is an unnamed child. Reading only the field would find no
/// anonymous class anywhere.
fn class_body_of<'r>(node: &SgNode<'r>) -> Option<SgNode<'r>> {
    node.field("body")
        .filter(|b| b.kind() == "class_body")
        .or_else(|| node.children().find(|c| c.kind() == "class_body"))
}

/// The enum type an enum constant's body is an anonymous subclass of (§8.9.3).
fn enclosing_enum(node: &SgNode) -> Option<Vec<String>> {
    node.ancestors()
        .find(|a| a.kind() == "enum_declaration")
        .and_then(|e| e.field("name"))
        .map(|n| vec![n.text().to_string()])
}

/// The erased type frame this node declares, if it declares one (T-03..T-05).
fn erased_frame(node: &SgNode) -> Option<ErasedType> {
    let kind = node.kind();
    let (body, superclass, interfaces) = if is_type_declaration(&kind) {
        // Exactly the declarations `collect_definitions` declines to make
        // nodes of: a local class, and any type declared inside something
        // that is not nameable itself — a member type of an anonymous class
        // is no more nameable than the class around it (§6.7).
        if !is_local_type(node) && owner_chain(node).is_some() {
            return None;
        }
        let mut superclass = None;
        let mut interfaces = Vec::new();
        for (role, head) in supertype_heads(node) {
            let segments = name_segments(&head);
            if segments.is_empty() {
                continue;
            }
            match role {
                SuperRole::Superclass => superclass = Some(segments),
                SuperRole::Interface => interfaces.push(segments),
            }
        }
        (class_body_of(node)?, superclass, interfaces)
    } else {
        // An anonymous class exists only when the creation site writes a
        // body; `new Base()` without one declares no type at all.
        let body = class_body_of(node)?;
        let superclass = match &*kind {
            "object_creation_expression" => {
                let named = name_segments(&node.field("type")?);
                (!named.is_empty()).then_some(named)
            }
            "enum_constant" => enclosing_enum(node),
            _ => return None,
        };
        (body, superclass, Vec::new())
    };
    let range = body.range();
    let (members, member_names) = frame_members(&body);
    Some(ErasedType {
        start: range.start as u32,
        end: range.end as u32,
        superclass,
        interfaces,
        members,
        member_names,
    })
}

/// Everything one node contributes to the file's definitions.
///
/// A definition whose [`owner_chain`] is `None` is skipped outright: it is
/// declared inside a type with no canonical name, so nothing anywhere could
/// name it (T-03, T-04).
fn collect_definitions(
    node: &SgNode,
    defs: &mut Vec<Definition>,
    inherit_heads: &mut HashSet<(usize, usize)>,
    types: &mut Vec<TypeDecl>,
) {
    let kind = node.kind();
    if is_type_declaration(&kind) {
        if is_local_type(node) {
            return;
        }
        let (Some(owner), Some(name)) = (owner_chain(node), node.field("name")) else {
            return;
        };
        let mut decl = TypeDecl::default();
        for (role, head) in supertype_heads(node) {
            let range = head.range();
            inherit_heads.insert((range.start, range.end));
            let segments = name_segments(&head);
            if segments.is_empty() {
                continue;
            }
            match role {
                SuperRole::Superclass => decl.superclass = Some(segments),
                SuperRole::Interface => decl.interfaces.push(segments),
            }
        }
        let name = name.text().to_string();
        let mut facets = modifier_facets(node)
            .union(type_kind_facets(&kind))
            .union(implicit_member_facets(node));
        // §8.9, §8.10, §9.1: a member enum, record, interface or annotation
        // type is implicitly static, however it is written.
        if !owner.is_empty() && !matches!(&*kind, "class_declaration") {
            facets = facets.union(DefFacets::STATIC);
        }
        defs.push(java_def(
            DefKind::Type,
            name.clone(),
            owner.clone(),
            DeclSpace::Type,
            facets,
            None,
            span_of(node),
        ));
        let mut own = owner;
        own.push(name);
        decl.path.clone_from(&own);
        types.push(decl);
        synthesize_members(node, &own, defs);
        return;
    }
    let Some(owner) = owner_chain(node) else {
        return;
    };
    match &*kind {
        "method_declaration" => {
            let Some(name) = node.field("name") else {
                return;
            };
            let params = parameters_of(node);
            let mut facets = modifier_facets(node).union(implicit_member_facets(node));
            if node.field("body").is_none() {
                facets = facets.union(DefFacets::ABSTRACT);
            }
            if params.as_ref().is_some_and(|p| p.varargs) {
                facets = facets.union(DefFacets::VARARGS);
            }
            defs.push(java_def(
                DefKind::Method,
                name.text().to_string(),
                owner,
                DeclSpace::Value,
                facets,
                params,
                span_of(node),
            ));
        }
        "annotation_type_element_declaration" => {
            // D-08: an annotation type element *is* a method (§9.6.1).
            let Some(name) = node.field("name") else {
                return;
            };
            defs.push(java_def(
                DefKind::Method,
                name.text().to_string(),
                owner,
                DeclSpace::Value,
                DefFacets::EXPORTED
                    .union(DefFacets::ABSTRACT)
                    .union(DefFacets::RUNTIME),
                parameters_of(node),
                span_of(node),
            ));
        }
        "constructor_declaration" | "compact_constructor_declaration" => {
            let Some(name) = node.field("name") else {
                return;
            };
            let params = parameters_of(node);
            let mut facets = modifier_facets(node);
            if params.as_ref().is_some_and(|p| p.varargs) {
                facets = facets.union(DefFacets::VARARGS);
            }
            defs.push(java_def(
                DefKind::Constructor,
                name.text().to_string(),
                owner,
                DeclSpace::Value,
                facets,
                params,
                span_of(node),
            ));
        }
        "field_declaration" | "constant_declaration" => {
            let facets = modifier_facets(node).union(implicit_member_facets(node));
            for declarator in node.field_children("declarator") {
                let Some(name) = declarator.field("name") else {
                    continue;
                };
                defs.push(java_def(
                    DefKind::Field,
                    name.text().to_string(),
                    owner.clone(),
                    DeclSpace::Value,
                    facets,
                    None,
                    span_of(&declarator),
                ));
            }
        }
        "enum_constant" => {
            // D-05: an enum constant is a public static final field (§8.9.1).
            // Its class body, if it has one, is an anonymous subclass and
            // declares nothing nameable (T-05, §8.9.3).
            let Some(name) = node.field("name") else {
                return;
            };
            defs.push(java_def(
                DefKind::Field,
                name.text().to_string(),
                owner,
                DeclSpace::Value,
                DefFacets::STATIC
                    .union(DefFacets::EXPORTED)
                    .union(DefFacets::RUNTIME),
                None,
                span_of(node),
            ));
        }
        _ => {}
    }
}

/// The declaration table an import consults, which is also the tier it sits
/// at during resolution.
fn import_space(kind: ImportKind) -> DeclSpace {
    match kind {
        ImportKind::SingleType => DeclSpace::Type,
        ImportKind::SingleStatic | ImportKind::StaticOnDemand => DeclSpace::Value,
        ImportKind::TypeOnDemand | ImportKind::Module => DeclSpace::Namespace,
    }
}

/// The dedup key text for an import site.
fn import_raw_target(import: &Import) -> String {
    let dotted = import.segments.join(".");
    match import.kind {
        ImportKind::SingleType | ImportKind::SingleStatic => dotted,
        ImportKind::TypeOnDemand | ImportKind::StaticOnDemand => format!("{dotted}.*"),
        ImportKind::Module => format!("module {dotted}"),
    }
}

/// Read one `import_declaration` into an [`Import`].
fn import_of(node: &SgNode) -> Option<Import> {
    let is_static = node.children().any(|c| c.kind() == "static");
    let on_demand = node.children().any(|c| c.kind() == "asterisk");
    let name_node = node
        .children()
        .find(|c| matches!(&*c.kind(), "identifier" | "scoped_identifier"))?;
    let mut segments = name_segments(&name_node);
    // `import module M;` (I-07, JEP 511) is not in this grammar version: it
    // parses with an ERROR node inside the name, and reading it through the
    // `scope`/`name` fields would silently drop a segment and call `module`
    // the first package name. Recognise the shape and recover the name from
    // the text rather than recording something false.
    if name_node.dfs().any(|n| n.kind() == "ERROR") {
        let raw = name_node.text().to_string();
        if let Some(rest) = raw.trim().strip_prefix("module") {
            let recovered: Vec<String> = rest
                .trim()
                .split('.')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !recovered.is_empty() {
                return Some(Import {
                    kind: ImportKind::Module,
                    segments: recovered,
                    span: span_of(node),
                });
            }
        }
    }
    if segments.is_empty() {
        return None;
    }
    // §7.5.4: a static-import-on-demand names the *owner type*, and the
    // grammar puts the type as the last segment either way.
    segments.shrink_to_fit();
    let kind = match (is_static, on_demand) {
        (true, true) => ImportKind::StaticOnDemand,
        (true, false) => ImportKind::SingleStatic,
        (false, true) => ImportKind::TypeOnDemand,
        (false, false) => ImportKind::SingleType,
    };
    Some(Import {
        kind,
        segments,
        span: span_of(node),
    })
}

/// A reference with the fields a header-level site always fills the same way:
/// nothing above a compilation unit is inside a binding environment, and
/// nothing there is a call.
fn header_ref(
    kind: RefKind,
    space: DeclSpace,
    raw_target: String,
    segments: Vec<String>,
    span: Span,
) -> Reference {
    Reference {
        kind,
        space,
        raw_target,
        target: RefTarget {
            root: TargetRoot::Name,
            segments,
        },
        locally_bound: false,
        argc: None,
        arg_types: None,
        enclosing: None,
        span,
    }
}

/// Everything one node contributes to the compilation-unit header, plus the
/// references those header facts are.
fn collect_header(
    node: &SgNode,
    header: &mut JavaHeader,
    refs: &mut Vec<Reference>,
    container_span: &mut Span,
) {
    match &*node.kind() {
        "package_declaration" => {
            // P-01: the package of a type is its `package` declaration, full
            // stop. §7.2 makes the package-to-directory mapping
            // implementation-specific, so the directory decides nothing.
            let segments = node
                .children()
                .find(|c| matches!(&*c.kind(), "identifier" | "scoped_identifier"))
                .map(|n| name_segments(&n))
                .unwrap_or_default();
            if !segments.is_empty() {
                header.package = Some(segments.join("."));
            }
            *container_span = span_of(node);
        }
        "module_declaration" => {
            // P-05: a module is a nameable thing and therefore a node.
            let Some(name) = node.field("name") else {
                return;
            };
            header.module = Some(name_segments(&name).join("."));
            *container_span = span_of(node);
        }
        "requires_module_directive" => {
            // I-08 / §7.7.1: the nearest thing Java has to `go.mod`'s
            // `require`, except that it names modules and not artifacts.
            let Some(module) = node.field("module") else {
                return;
            };
            let segments = name_segments(&module);
            refs.push(header_ref(
                RefKind::Import,
                DeclSpace::Namespace,
                segments.join("."),
                segments,
                span_of(node),
            ));
        }
        "exports_module_directive" => {
            let Some(package) = node.field("package") else {
                return;
            };
            let segments = name_segments(&package);
            refs.push(header_ref(
                RefKind::Export,
                DeclSpace::Namespace,
                segments.join("."),
                segments,
                span_of(node),
            ));
        }
        "import_declaration" => {
            let Some(import) = import_of(node) else {
                return;
            };
            // I-10: unused, duplicate and same-package imports are all legal
            // and all still references. Classifying them is the resolver's;
            // skipping them would lower the denominator the rate is measured
            // against.
            refs.push(header_ref(
                RefKind::Import,
                import_space(import.kind),
                import_raw_target(&import),
                import.segments.clone(),
                import.span,
            ));
            header.imports.push(import);
        }
        _ => {}
    }
}

/// The declaration table a reference site reads, for the purpose of asking
/// whether a local shadows its leftmost name.
fn reference_namespace(kind: RefKind) -> Option<Namespace> {
    match kind {
        RefKind::Call | RefKind::FieldAccess => Some(Namespace::Value),
        // A creation site names a *type*; a local class can shadow it.
        RefKind::New | RefKind::TypeUse | RefKind::Inherit | RefKind::Annotation => {
            Some(Namespace::Type)
        }
        _ => None,
    }
}

/// The extractor's file-local verdict: some enclosing region binds the
/// reference's leftmost name with a declaration that is not a node.
///
/// A *fact*, not a decision — the resolver still owns the outcome, and for
/// Java it usually does something better with it than give up: a local with a
/// declared type is exactly what X-02's member lookup runs on.
fn is_locally_bound(header: &JavaHeader, kind: RefKind, target: &RefTarget, site: u32) -> bool {
    if !matches!(target.root, TargetRoot::Name) {
        return false;
    }
    let Some(root) = target.segments.first() else {
        return false;
    };
    // §6.5.1: an identifier immediately before `(` is a MethodName and can
    // only denote a method, so a local named `foo` does not interfere with
    // `foo()`. Java is easier than Go here, and reading it the Go way would
    // move real edges into the local bucket — deleting them from *both* terms
    // of the resolution rate.
    if kind == RefKind::Call && target.segments.len() == 1 {
        return false;
    }
    match kind {
        // §15.13: `Foo::bar`'s receiver is a type or an expression and only
        // the symbol table decides which, so a binding in either table counts.
        RefKind::MethodRef => {
            header.binds_locally(root, site, Namespace::Value)
                || header.binds_locally(root, site, Namespace::Type)
        }
        _ => reference_namespace(kind).is_some_and(|ns| header.binds_locally(root, site, ns)),
    }
}

/// Assemble one reference, computing the two fields every site shares.
fn reference(
    node: &SgNode,
    header: &JavaHeader,
    kind: RefKind,
    space: DeclSpace,
    raw_target: String,
    target: RefTarget,
    argc: Option<u32>,
) -> Reference {
    let site = node.range().start as u32;
    Reference {
        kind,
        space,
        raw_target,
        locally_bound: is_locally_bound(header, kind, &target, site),
        target,
        argc,
        arg_types: matches!(kind, RefKind::Call | RefKind::New)
            .then(|| argument_types(node, header))
            .flatten(),
        enclosing: enclosing_definition(node),
        span: span_of(node),
    }
}

/// Whether a `field_access` node is a reference of its own.
///
/// It is not when it is only the qualifier of a longer chain — the enclosing
/// call, method reference or access already carries those segments — and not
/// when its `field` is `this` or `super`, which mark a *root* (§15.8.4,
/// §15.12.1) rather than select a member.
fn field_access_is_a_site(node: &SgNode) -> bool {
    if node
        .field("field")
        .is_some_and(|f| matches!(&*f.kind(), "this" | "super"))
    {
        return false;
    }
    match node.parent() {
        Some(parent) => match &*parent.kind() {
            "field_access" | "method_reference" => false,
            "method_invocation" => parent
                .field("object")
                .is_none_or(|object| object.range() != node.range()),
            _ => true,
        },
        None => true,
    }
}

/// Whether a node was recovered from inside a region tree-sitter could not
/// parse.
///
/// tree-sitter is error-tolerant, which is what lets a file with one bad line
/// still yield every reference on the other lines — but recovery also invents
/// structure inside text that is not code. A fuzz-corpus string literal in
/// commons-lang carries control bytes that break the literal, and recovery
/// reads `$${.u` out of the wreckage as a `type_identifier`: one row, 405
/// occurrences, `External("$$")`. A reference is defined as *a site in one
/// file*, and there is no site here — the bytes are inside a string.
///
/// Ancestors only: an `ERROR` elsewhere in the file says nothing about a node
/// that parsed.
fn in_error_region(node: &SgNode) -> bool {
    node.ancestors().any(|a| a.kind() == "ERROR")
}

/// Everything one node contributes to the file's references.
fn collect_references(
    source: &str,
    node: &SgNode,
    header: &JavaHeader,
    inherit_heads: &HashSet<(usize, usize)>,
    refs: &mut Vec<Reference>,
) {
    if in_error_region(node) {
        return;
    }
    match &*node.kind() {
        "method_invocation" => {
            let Some(name) = node.field("name") else {
                return;
            };
            let target = call_target(node);
            refs.push(reference(
                node,
                header,
                RefKind::Call,
                DeclSpace::Value,
                call_raw_target(source, node, name.range().end),
                target,
                argument_count(node),
            ));
        }
        "object_creation_expression" => {
            // C-01: two references in one expression. This is the constructor
            // half; the type half is the `type_identifier` beneath, which the
            // type-use arm emits on its own.
            //
            // C-04 normalises both `outer.new Inner()` and `new Outer.Inner()`
            // for free: the enclosing instance is not part of the target, and
            // the grammar keeps only the type in the `type` field.
            let Some(type_node) = node.field("type") else {
                return;
            };
            let segments = name_segments(&type_node);
            if segments.is_empty() {
                return;
            }
            refs.push(reference(
                node,
                header,
                RefKind::New,
                DeclSpace::Value,
                compact(&type_node.text()),
                RefTarget {
                    root: TargetRoot::Name,
                    segments,
                },
                argument_count(node),
            ));
        }
        "explicit_constructor_invocation" => {
            // C-03: `this(…)` and `super(…)` are call sites naming
            // `ThisType#<init>` and `SuperType#<init>`, and both are fully
            // resolvable — no inference anywhere.
            let Some(constructor) = node.field("constructor") else {
                return;
            };
            let qualifier = node
                .children()
                .find(|c| matches!(&*c.kind(), "identifier" | "field_access"))
                .map(|q| access_chain(&q).segments)
                .unwrap_or_default();
            let root = match &*constructor.kind() {
                "this" => TargetRoot::This { qualifier },
                "super" => TargetRoot::Super { qualifier },
                _ => return,
            };
            refs.push(reference(
                node,
                header,
                RefKind::New,
                DeclSpace::Value,
                constructor.text().to_string(),
                RefTarget {
                    root,
                    segments: Vec::new(),
                },
                argument_count(node),
            ));
        }
        "enum_constant" => {
            // §8.9.1: a constant with an argument list invokes the enum's
            // constructor at a site written in the source. One with no
            // arguments has no site, and C-09 forbids inventing one.
            if node.field("arguments").is_none() {
                return;
            }
            let Some(owner) = node
                .ancestors()
                .find(|a| a.kind() == "enum_declaration")
                .and_then(|e| e.field("name"))
            else {
                return;
            };
            let name = owner.text().to_string();
            refs.push(reference(
                node,
                header,
                RefKind::New,
                DeclSpace::Value,
                name.clone(),
                RefTarget {
                    root: TargetRoot::Name,
                    segments: vec![name],
                },
                argument_count(node),
            ));
        }
        "method_reference" => {
            // C-08: `Foo::new` names a constructor and `Foo::bar` a method,
            // but *which* overload is chosen by the target functional
            // interface type (§15.13.1) — target typing, not inference on the
            // receiver, which is why the arity is `None` rather than zero.
            let children: Vec<SgNode> = node.children().collect();
            let Some(separator) = children.iter().position(|c| c.kind() == "::") else {
                return;
            };
            let receiver = children[..separator].iter().rev().find(|c| c.is_named());
            let Some(tail) = children[separator + 1..]
                .iter()
                .find(|c| c.kind() == "new" || c.kind() == "identifier")
            else {
                return;
            };
            let mut chain = receiver.map_or_else(expr_chain, access_chain);
            // `<init>` cannot be a Java identifier (§3.8 excludes `<` and
            // `>`), so it names the constructor unambiguously and matches the
            // FQN grammar M-02 already uses.
            chain.segments.push(if tail.kind() == "new" {
                "<init>".to_string()
            } else {
                tail.text().to_string()
            });
            refs.push(reference(
                node,
                header,
                RefKind::MethodRef,
                DeclSpace::Value,
                compact(&node.text()),
                RefTarget {
                    root: chain.root,
                    segments: chain.segments,
                },
                None,
            ));
        }
        "annotation" | "marker_annotation" => {
            let Some(name) = node.field("name") else {
                return;
            };
            let segments = name_segments(&name);
            if segments.is_empty() {
                return;
            }
            refs.push(reference(
                node,
                header,
                RefKind::Annotation,
                DeclSpace::Type,
                compact(&name.text()),
                RefTarget {
                    root: TargetRoot::Name,
                    segments,
                },
                None,
            ));
        }
        "field_access" => {
            if !field_access_is_a_site(node) {
                return;
            }
            let chain = access_chain(node);
            if chain.segments.is_empty() {
                return;
            }
            refs.push(reference(
                node,
                header,
                RefKind::FieldAccess,
                DeclSpace::Value,
                compact(&node.text()),
                RefTarget {
                    root: chain.root,
                    segments: chain.segments,
                },
                None,
            ));
        }
        "type_identifier" | "scoped_type_identifier" => {
            // A segment of a longer scoped name is not a reference of its
            // own — the outermost node carries the whole path (N-04).
            if node
                .parent()
                .is_some_and(|p| matches!(&*p.kind(), "scoped_type_identifier" | "type_parameter"))
            {
                return;
            }
            // `var` is a reserved type name (§14.4), not a type.
            if node.text() == "var" {
                return;
            }
            let segments = name_segments(node);
            if segments.is_empty() {
                return;
            }
            let range = node.range();
            let kind = if inherit_heads.contains(&(range.start, range.end)) {
                RefKind::Inherit
            } else {
                RefKind::TypeUse
            };
            refs.push(reference(
                node,
                header,
                kind,
                DeclSpace::Type,
                compact(&node.text()),
                RefTarget {
                    root: TargetRoot::Name,
                    segments,
                },
                None,
            ));
        }
        _ => {}
    }
}

/// Split every callable this file declares into overload groups, name shared
/// groups, and give unique callables a forwarding signature alias.
///
/// M-01: two overloads are two definitions and must be two nodes. M-04: a
/// call site knows only the callee's name and argument count, so the identity
/// it can construct is `type.name/argc` — which two overloads of one arity
/// cannot both own. So neither takes it: both fall back to the signature form
/// and the shared key becomes a [`DefKind::Alias`] node standing for the set.
/// A call landing on it has found an ambiguity to report rather than a target
/// to guess at.
///
/// Complete per type by construction: §7.6 and §8.1 put every member of a
/// type in one compilation unit, so a file sees every declaration that could
/// compete for one of its own types' keys.
///
/// Returns the shared keys, which [`crate::lang::Resolver::def_fqn`] reads to
/// decide which form a callable definition's FQN takes.
fn mark_overload_sets(defs: &mut Vec<Definition>) -> HashSet<String> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for def in defs.iter() {
        if let Some(key) = group_key(def) {
            *counts.entry(key).or_default() += 1;
        }
    }
    let shared: HashSet<String> = counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|(key, _)| key)
        .collect();
    let mut aliases: Vec<Definition> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for def in defs.iter() {
        let Some(key) = group_key(def) else { continue };
        let callable = fqn::callable_of(def);
        if !shared.contains(&key) {
            // The arity identity remains the callable's node. A forwarding
            // signature identity exposes its parameter shape to typed
            // applicability without re-aiming existing edges.
            aliases.push(java_def(
                DefKind::Alias,
                callable.signature(),
                def.owner.clone(),
                DeclSpace::Value,
                DefFacets::SYNTHETIC.union(DefFacets::RUNTIME),
                None,
                def.span,
            ));
            continue;
        }
        if !seen.insert(key) {
            continue;
        }
        aliases.push(java_def(
            DefKind::Alias,
            callable.key(),
            def.owner.clone(),
            DeclSpace::Value,
            DefFacets::SYNTHETIC.union(DefFacets::RUNTIME),
            None,
            def.span,
        ));
    }
    defs.extend(aliases);
    shared
}

/// The overload group a definition competes in, or `None` when it is not a
/// callable and competes in none.
fn group_key(def: &Definition) -> Option<String> {
    if !matches!(def.kind, DefKind::Method | DefKind::Constructor) {
        return None;
    }
    let callable = fqn::callable_of(def);
    Some(fqn::overload_group(
        &def.owner,
        &callable.name,
        callable.count(),
        callable.varargs,
    ))
}

/// Extract all facts from one Java source file.
///
/// Two passes over one match list, and the order is load-bearing: a
/// reference's `locally_bound` is a question about the *whole* file's binding
/// environment, and a binder can sit textually after the site it does not
/// bind. Collecting bindings first is what lets the second pass answer with
/// extents rather than with presence.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<JavaLang> {
    let tree = SourceTree::parse_java(source);
    let matched = tree.matches(rules());
    let mut header = JavaHeader {
        rel_path: rel_path.to_string(),
        ..JavaHeader::default()
    };
    let mut defs: Vec<Definition> = Vec::new();
    let mut refs: Vec<Reference> = Vec::new();
    let mut inherit_heads: HashSet<(usize, usize)> = HashSet::new();
    let mut types: Vec<TypeDecl> = Vec::new();
    let mut container_span = Span {
        byte_start: 0,
        byte_end: 0,
        line: 0,
    };

    for (_, node) in &matched {
        collect_header(node, &mut header, &mut refs, &mut container_span);
        collect_bindings(node, &mut header.bindings);
        collect_definitions(node, &mut defs, &mut inherit_heads, &mut types);
        // T-03..T-05: the frames `collect_definitions` declines to make nodes
        // of, recorded so the resolver can tell "inside an unnameable type"
        // from "inside the type around it".
        if let Some(frame) = erased_frame(node) {
            header.erased.push(frame);
        }
    }
    header.types = types;
    header.overloaded = mark_overload_sets(&mut defs);
    for (_, node) in &matched {
        collect_references(source, node, &header, &inherit_heads, &mut refs);
    }

    // The file's container definition, emitted whether or not a package
    // clause parsed: a file that lost its clause still belongs somewhere, and
    // the container node is what a reference with no nameable encloser
    // sources from. An empty name means "this file does not say", which is
    // not the same as naming the empty string — P-03's unnamed package is
    // exactly that (§7.4.2), and nothing outside it can name its types
    // because §7.5 requires a qualified name to import one.
    let container = header
        .module
        .clone()
        .or_else(|| header.package.clone())
        .unwrap_or_default();
    defs.insert(
        0,
        java_def(
            DefKind::Module,
            container,
            Vec::new(),
            DeclSpace::Namespace,
            DefFacets::RUNTIME,
            None,
            container_span,
        ),
    );

    FileFacts { header, defs, refs }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(source: &str) -> FileFacts<JavaLang> {
        extract("src/main/java/com/acme/A.java", source)
    }

    /// Wrap statements in the smallest legal compilation unit.
    fn in_method(body: &str) -> String {
        format!("package p;\n\nclass A {{\n    void f() {{\n{body}\n    }}\n}}\n")
    }

    fn refs_of(f: &FileFacts<JavaLang>, kind: RefKind) -> Vec<&Reference> {
        f.refs.iter().filter(|r| r.kind == kind).collect()
    }

    fn site<'f>(f: &'f FileFacts<JavaLang>, kind: RefKind, raw: &str) -> &'f Reference {
        f.refs
            .iter()
            .find(|r| r.kind == kind && r.raw_target == raw)
            .unwrap_or_else(|| panic!("no {kind:?} site `{raw}` in {:?}", f.refs))
    }

    fn bound(f: &FileFacts<JavaLang>, kind: RefKind, raw: &str) -> bool {
        site(f, kind, raw).locally_bound
    }

    fn def<'f>(f: &'f FileFacts<JavaLang>, name: &str) -> &'f Definition {
        f.defs
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("no definition named `{name}`: {:?}", f.defs))
    }

    fn defs_named<'f>(f: &'f FileFacts<JavaLang>, name: &str) -> Vec<&'f Definition> {
        f.defs.iter().filter(|d| d.name == name).collect()
    }

    fn segments(r: &Reference) -> Vec<&str> {
        r.target.segments.iter().map(String::as_str).collect()
    }

    fn binding<'f>(f: &'f FileFacts<JavaLang>, name: &str) -> &'f Binding {
        f.header
            .bindings
            .iter()
            .find(|b| b.name == name)
            .unwrap_or_else(|| panic!("no binding named `{name}`"))
    }

    // ---------------------------------------------------------------
    // Binding environments (N-01, N-05, N-06, N-07, X-02, X-03, §4.4)
    // ---------------------------------------------------------------

    #[test]
    fn a_bare_call_is_never_shadowed_by_a_local() {
        // N-01 / §6.5.1: an identifier immediately before `(` is a MethodName
        // and can only denote a method, so a local named `run` does not
        // interfere with `run()`. Reading this the Go way would move a real
        // edge into the local bucket, deleting it from *both* terms of the
        // resolution rate.
        let f = facts(&in_method("        Runnable run = null;\n        run();"));
        assert!(!bound(&f, RefKind::Call, "run"));
        assert_eq!(binding(&f, "run").kind, BindingKind::Local);
    }

    #[test]
    fn a_local_qualifier_is_locally_bound_and_states_its_type() {
        // X-02, the biggest Java lever: `f.m()` names a member of a type that
        // is *written down in this file*. The extractor states the binding and
        // the declared type; the resolver still owns the outcome, and giving
        // up here would floor the rate because almost every Java call is a
        // receiver call.
        let f = facts(&in_method("        Foo thing = null;\n        thing.m();"));
        assert!(bound(&f, RefKind::Call, "thing.m"));
        let b = binding(&f, "thing");
        assert_eq!(b.kind, BindingKind::Local);
        assert_eq!(
            b.declared_type.as_deref(),
            Some(["Foo".to_string()].as_slice())
        );
    }

    #[test]
    fn a_binding_before_the_site_binds_it_and_one_after_does_not() {
        // §6.3 starts a local's scope at its own declaration, so position
        // decides and not presence.
        let f = facts(&in_method(
            "        thing.m();\n        Foo thing = null;\n        thing.m();",
        ));
        let seen: Vec<bool> = refs_of(&f, RefKind::Call)
            .iter()
            .filter(|r| r.raw_target == "thing.m")
            .map(|r| r.locally_bound)
            .collect();
        assert_eq!(seen, [false, true]);
    }

    #[test]
    fn a_sibling_block_binding_does_not_escape() {
        let f = facts(&in_method(
            "        if (true) {\n            Foo thing = null;\n            thing.m();\n        }\n        thing.m();",
        ));
        let seen: Vec<bool> = refs_of(&f, RefKind::Call)
            .iter()
            .filter(|r| r.raw_target == "thing.m")
            .map(|r| r.locally_bound)
            .collect();
        assert_eq!(seen, [true, false], "only the enclosing block binds");
    }

    #[test]
    fn parameters_bind_their_whole_body_and_carry_their_type() {
        let f = facts(
            "package p;\n\nclass A {\n    void f(Foo thing, int... rest) {\n        thing.m();\n        rest.clone();\n    }\n}\n",
        );
        assert!(bound(&f, RefKind::Call, "thing.m"));
        assert_eq!(binding(&f, "thing").kind, BindingKind::Parameter);
        assert_eq!(
            binding(&f, "thing").declared_type.as_deref(),
            Some(["Foo".to_string()].as_slice())
        );
        // §8.4.1 makes a variable-arity parameter's type an array type, and
        // this model holds no array members, so it states none rather than
        // stating the element type.
        assert!(bound(&f, RefKind::Call, "rest.clone"));
        assert_eq!(binding(&f, "rest").declared_type, None);
    }

    #[test]
    fn a_field_is_in_the_environment_but_never_makes_a_site_local() {
        // A field *is* a node (D-05). Marking `count.m()` local would delete a
        // resolvable reference from both terms of the rate.
        let f = facts(
            "package p;\n\nclass A {\n    Foo count;\n\n    void f() {\n        count.m();\n    }\n}\n",
        );
        assert!(!bound(&f, RefKind::Call, "count.m"));
        let b = binding(&f, "count");
        assert_eq!(b.kind, BindingKind::Field);
        assert!(!b.kind.is_local());
        assert_eq!(
            b.declared_type.as_deref(),
            Some(["Foo".to_string()].as_slice())
        );
    }

    #[test]
    fn a_field_binds_the_whole_class_body_whatever_the_order() {
        // §8.3.3: forward reference to a field is legal from a method body.
        let f = facts(
            "package p;\n\nclass A {\n    void f() {\n        later.m();\n    }\n\n    Foo later;\n}\n",
        );
        assert_eq!(
            binding(&f, "later").declared_type.as_deref(),
            Some(["Foo".to_string()].as_slice())
        );
        let b = binding(&f, "later");
        let call = site(&f, RefKind::Call, "later.m");
        assert!(b.start <= call.span.byte_start && call.span.byte_start < b.end);
    }

    #[test]
    fn a_pattern_variable_binds_conservatively_for_its_statement() {
        // N-06 / §6.3.1: flow scoping. Over-binding a *declared type* is safe
        // because §6.4.1 forbids a conflicting declaration in the region.
        let f = facts(&in_method(
            "        if (o instanceof Foo ff) {\n            ff.m();\n        }",
        ));
        assert!(bound(&f, RefKind::Call, "ff.m"));
        let b = binding(&f, "ff");
        assert_eq!(b.kind, BindingKind::PatternVariable);
        assert_eq!(
            b.declared_type.as_deref(),
            Some(["Foo".to_string()].as_slice())
        );
    }

    #[test]
    fn a_switch_pattern_binds_its_own_case_only() {
        let f = facts(&in_method(
            "        switch (o) {\n            case Foo g -> g.m();\n            default -> nothing();\n        }",
        ));
        assert!(bound(&f, RefKind::Call, "g.m"));
        assert_eq!(
            binding(&f, "g").declared_type.as_deref(),
            Some(["Foo".to_string()].as_slice())
        );
    }

    #[test]
    fn catch_parameters_bind_and_a_multi_catch_states_no_type() {
        let f = facts(&in_method(
            "        try {\n            risky();\n        } catch (IOException e) {\n            e.printStackTrace();\n        }\n        try {\n            risky();\n        } catch (IOException | SQLException g) {\n            g.printStackTrace();\n        }",
        ));
        assert!(bound(&f, RefKind::Call, "e.printStackTrace"));
        assert_eq!(binding(&f, "e").kind, BindingKind::CatchParameter);
        assert_eq!(
            binding(&f, "e").declared_type.as_deref(),
            Some(["IOException".to_string()].as_slice())
        );
        // §14.20: a multi-catch parameter's type is a least upper bound, which
        // is written nowhere, so no type is claimed.
        assert!(bound(&f, RefKind::Call, "g.printStackTrace"));
        assert_eq!(binding(&f, "g").declared_type, None);
    }

    #[test]
    fn resources_and_enhanced_for_variables_bind() {
        let f = facts(&in_method(
            "        try (Reader r = open()) {\n            r.read();\n        }\n        for (Item it : items) {\n            it.use();\n        }",
        ));
        assert!(bound(&f, RefKind::Call, "r.read"));
        assert_eq!(
            binding(&f, "r").declared_type.as_deref(),
            Some(["Reader".to_string()].as_slice())
        );
        assert!(bound(&f, RefKind::Call, "it.use"));
        assert_eq!(
            binding(&f, "it").declared_type.as_deref(),
            Some(["Item".to_string()].as_slice())
        );
        // The resource's own initializer is a call to something else.
        assert!(!bound(&f, RefKind::Call, "open"));
    }

    #[test]
    fn lambda_parameters_bind_with_and_without_a_declared_type() {
        let f = facts(&in_method(
            "        items.forEach(x -> x.use());\n        items.forEach((Item y) -> y.use());",
        ));
        assert!(bound(&f, RefKind::Call, "x.use"));
        assert_eq!(binding(&f, "x").declared_type, None);
        assert!(bound(&f, RefKind::Call, "y.use"));
        assert_eq!(
            binding(&f, "y").declared_type.as_deref(),
            Some(["Item".to_string()].as_slice())
        );
    }

    #[test]
    fn var_is_read_by_shape_and_nothing_deeper() {
        // X-03: `var x = new Foo();` states its type in this file; `var x =
        // f();` needs the callee's return type, which is inference and not
        // this tool's tier.
        let f = facts(&in_method(
            "        var made = new Foo();\n        var cast = (Bar) o;\n        var chained = f();\n        made.m();\n        cast.m();\n        chained.m();",
        ));
        assert_eq!(
            binding(&f, "made").declared_type.as_deref(),
            Some(["Foo".to_string()].as_slice())
        );
        assert_eq!(
            binding(&f, "cast").declared_type.as_deref(),
            Some(["Bar".to_string()].as_slice())
        );
        assert_eq!(binding(&f, "chained").declared_type, None);
        // All three are still locally bound: the fact is the binding, not the
        // type. A missing type is `NeedsTypeInference` — `NeedsReceiverType`
        // is the case where the type *is* stated and in the repository, which
        // is a lookup rather than a reported failure.
        for raw in ["made.m", "cast.m", "chained.m"] {
            assert!(bound(&f, RefKind::Call, raw));
        }
        // `var` is not a type name and must not become a type reference.
        assert!(
            !refs_of(&f, RefKind::TypeUse)
                .iter()
                .any(|r| r.raw_target == "var")
        );
    }

    #[test]
    fn a_type_parameter_shadows_a_type_name_and_a_local_does_not() {
        // §4.4: a type parameter's scope is its declaration and it is never a
        // node, so `T` here must not be linked to some class `T`. §6.5.1 keeps
        // the value and type namespaces apart, which is why a local called
        // `Foo` leaves the *type* `Foo` alone.
        let f = facts(
            "package p;\n\nclass A {\n    <T extends Number> void f(T t) {\n        T local = null;\n        Foo Foo = null;\n        Foo other = null;\n    }\n}\n",
        );
        assert_eq!(binding(&f, "T").kind, BindingKind::TypeParameter);
        assert!(bound(&f, RefKind::TypeUse, "T"));
        // The bound is a real type use and is not the parameter's own name.
        assert!(!bound(&f, RefKind::TypeUse, "Number"));
        assert!(!bound(&f, RefKind::TypeUse, "Foo"));
    }

    #[test]
    fn a_local_class_shadows_an_imported_type() {
        // N-05: a local class has no canonical name (§6.7), so a type use
        // naming it must not be linked to the import of the same simple name.
        let f = facts(
            "package p;\n\nimport q.Helper;\n\nclass A {\n    void f() {\n        class Helper {}\n        Helper h = null;\n    }\n\n    void g() {\n        Helper h = null;\n    }\n}\n",
        );
        let uses: Vec<bool> = refs_of(&f, RefKind::TypeUse)
            .iter()
            .filter(|r| r.raw_target == "Helper")
            .map(|r| r.locally_bound)
            .collect();
        assert_eq!(uses, [true, false], "the local class binds only its block");
        assert_eq!(binding(&f, "Helper").kind, BindingKind::LocalType);
    }

    #[test]
    fn the_unnamed_variable_binds_nothing() {
        // N-07 / JEP 456: `_` declares nothing nameable, so it is never a
        // binding and never a shadow.
        let f = facts(&in_method(
            "        for (var _ : items) {\n            nothing();\n        }",
        ));
        assert!(
            !f.header.bindings.iter().any(|b| b.name == "_"),
            "`_` bound something: {:?}",
            f.header.bindings
        );
    }

    #[test]
    fn a_capture_inside_an_anonymous_class_is_still_bound() {
        // Extents, not ancestry: a lambda or anonymous body sits inside the
        // method's byte range, so a captured local binds there too.
        let f = facts(&in_method(
            "        Foo thing = null;\n        Runnable r = new Runnable() {\n            public void run() {\n                thing.m();\n            }\n        };",
        ));
        assert!(bound(&f, RefKind::Call, "thing.m"));
    }

    // ---------------------------------------------------------------
    // Compilation units, packages and imports (P-01..P-05, I-01..I-10)
    // ---------------------------------------------------------------

    const UNIT: &str = r#"package com.acme.util;

import java.util.List;
import java.util.*;
import static org.junit.Assert.assertEquals;
import static java.util.Arrays.*;
import java.util.Map.Entry;
import java.util.List;

class Helper {}

public class Outer {}
"#;

    #[test]
    fn the_package_is_the_declaration_and_not_the_directory() {
        // P-01 / §7.2: the package-to-directory mapping is
        // implementation-specific, so a file under `src/main/java/com/acme`
        // declaring `package com.acme.util;` is in `com.acme.util`.
        let f = facts(UNIT);
        assert_eq!(f.header.package.as_deref(), Some("com.acme.util"));
        assert_eq!(f.header.rel_path, "src/main/java/com/acme/A.java");
        // The container definition is the package, and it is `defs[0]` so the
        // driver finds it wherever a reference has no nameable encloser.
        assert_eq!(f.defs[0].kind, DefKind::Module);
        assert_eq!(f.defs[0].name, "com.acme.util");
        assert_eq!(f.defs[0].space, DeclSpace::Namespace);
    }

    #[test]
    fn the_unnamed_package_says_nothing_rather_than_naming_nothing() {
        // P-03 / §7.4.2. An empty container name means "this file does not
        // say", which is not the same as naming the empty string.
        let f = facts(
            "class A {}
",
        );
        assert_eq!(f.header.package, None);
        assert_eq!(f.defs[0].kind, DefKind::Module);
        assert_eq!(f.defs[0].name, "");
    }

    #[test]
    fn several_top_level_types_in_one_compilation_unit() {
        // P-04 / §7.6: at most one may be public; the rest are package-private
        // and still top-level, with plain `pkg.Name` binary names.
        let f = facts(UNIT);
        assert!(def(&f, "Helper").owner.is_empty());
        assert!(def(&f, "Outer").owner.is_empty());
        assert!(!def(&f, "Helper").facets.contains(DefFacets::EXPORTED));
        assert!(def(&f, "Outer").facets.contains(DefFacets::EXPORTED));
    }

    #[test]
    fn every_import_form_is_recorded_with_its_tier() {
        // I-01..I-05, I-09: the *form* decides the candidate tier (N-03), so a
        // bool for `static` would not be enough.
        let f = facts(UNIT);
        let forms: Vec<(ImportKind, String)> = f
            .header
            .imports
            .iter()
            .map(|i| (i.kind, i.segments.join(".")))
            .collect();
        assert_eq!(
            forms,
            [
                (ImportKind::SingleType, "java.util.List".to_string()),
                (ImportKind::TypeOnDemand, "java.util".to_string()),
                (
                    ImportKind::SingleStatic,
                    "org.junit.Assert.assertEquals".to_string()
                ),
                (ImportKind::StaticOnDemand, "java.util.Arrays".to_string()),
                // I-09: the package/type split in `java.util.Map.Entry` is not
                // decidable lexically — `java.util.Map` could have been a
                // package — so the extractor segments and the resolver splits.
                (ImportKind::SingleType, "java.util.Map.Entry".to_string()),
                (ImportKind::SingleType, "java.util.List".to_string()),
            ]
        );
    }

    #[test]
    fn imports_are_references_and_header_entries() {
        // I-10: unused, duplicate and same-package imports are all legal and
        // all still references. Skipping the duplicate would lower the
        // denominator the resolution rate is measured against.
        let f = facts(UNIT);
        let imports = refs_of(&f, RefKind::Import);
        assert_eq!(imports.len(), f.header.imports.len());
        for (r, i) in imports.iter().zip(&f.header.imports) {
            assert_eq!(
                segments(r),
                i.segments.iter().map(String::as_str).collect::<Vec<_>>()
            );
            assert_eq!(r.span, i.span);
            assert_eq!(r.enclosing, None);
            assert!(!r.locally_bound);
            assert_eq!(r.argc, None);
        }
        // The two `java.util.List` sites are one dedup row and the on-demand
        // one is not confusable with them.
        let raws: Vec<&str> = imports.iter().map(|r| r.raw_target.as_str()).collect();
        assert_eq!(raws.iter().filter(|t| **t == "java.util.List").count(), 2);
        assert!(raws.contains(&"java.util.*"));
        assert!(raws.contains(&"java.util.Arrays.*"));
        // A static import reads the value table; a type import the type one.
        assert_eq!(
            site(&f, RefKind::Import, "org.junit.Assert.assertEquals").space,
            DeclSpace::Value
        );
        assert_eq!(
            site(&f, RefKind::Import, "java.util.List").space,
            DeclSpace::Type
        );
        assert_eq!(
            site(&f, RefKind::Import, "java.util.*").space,
            DeclSpace::Namespace
        );
    }

    #[test]
    fn module_info_declares_a_module_and_its_directives_are_references() {
        // P-05: a module is a nameable thing and therefore a node. I-08:
        // `requires` names modules and `exports` names packages.
        let f = extract(
            "module-info.java",
            "module com.acme.app {
    requires com.other;
    exports com.acme.api;
}
",
        );
        assert_eq!(f.header.module.as_deref(), Some("com.acme.app"));
        assert_eq!(f.header.package, None);
        assert_eq!(f.defs[0].kind, DefKind::Module);
        assert_eq!(f.defs[0].name, "com.acme.app");
        assert_eq!(
            segments(site(&f, RefKind::Import, "com.other")),
            ["com", "other"]
        );
        assert_eq!(
            segments(site(&f, RefKind::Export, "com.acme.api")),
            ["com", "acme", "api"]
        );
    }

    #[test]
    fn a_module_import_is_recognised_even_though_the_grammar_errors_on_it() {
        // I-07 / JEP 511. This grammar version parses `import module M;` with
        // an ERROR node inside the name, and reading it through the
        // `scope`/`name` fields would silently drop a segment and record
        // `module` as the first package name. Recognising the shape is the
        // difference between an honest low-priority gap and a false fact.
        let f = facts(
            "import module java.base;

class A {}
",
        );
        assert_eq!(f.header.imports.len(), 1);
        assert_eq!(f.header.imports[0].kind, ImportKind::Module);
        assert_eq!(f.header.imports[0].segments, ["java", "base"]);
        assert_eq!(
            site(&f, RefKind::Import, "module java.base").space,
            DeclSpace::Namespace
        );
    }

    // ---------------------------------------------------------------
    // Types as scopes and the node rule (T-01..T-07, D-01..D-11)
    // ---------------------------------------------------------------

    const TYPES: &str = r#"package p;

public class Outer {
    static class Inner {
        void deep() {}
    }

    class NonStatic {}

    interface Contract {
        int LIMIT = 1;

        void required();

        default void provided() {}

        static void helper() {}
    }
}

enum Color {
    RED,
    GREEN {
        void hidden() {}
    };

    void shared() {}
}

record Point(int x, int y) {}

@interface Anno {
    String value();
}
"#;

    #[test]
    fn nested_types_carry_their_enclosing_chain() {
        // T-01, T-02: both are nameable as `Outer.Inner`; they differ only in
        // construction and in having an enclosing instance.
        let f = facts(TYPES);
        assert_eq!(def(&f, "Inner").owner, ["Outer"]);
        assert!(def(&f, "Inner").facets.contains(DefFacets::STATIC));
        assert_eq!(def(&f, "NonStatic").owner, ["Outer"]);
        assert!(!def(&f, "NonStatic").facets.contains(DefFacets::STATIC));
        assert_eq!(def(&f, "deep").owner, ["Outer", "Inner"]);
        // §9.5: a member type of an interface is implicitly public and static.
        assert!(def(&f, "Contract").facets.contains(DefFacets::INTERFACE));
        assert!(def(&f, "Contract").facets.contains(DefFacets::STATIC));
    }

    #[test]
    fn the_kind_of_a_type_is_a_facet_and_not_a_kind() {
        let f = facts(TYPES);
        for (name, facet) in [
            ("Color", DefFacets::ENUM),
            ("Point", DefFacets::RECORD),
            ("Anno", DefFacets::ANNOTATION),
            ("Contract", DefFacets::INTERFACE),
        ] {
            let d = def(&f, name);
            assert_eq!(d.kind, DefKind::Type, "{name}");
            assert_eq!(d.space, DeclSpace::Type, "{name}");
            assert!(d.facets.contains(facet), "{name}");
        }
    }

    #[test]
    fn interface_members_are_public_and_the_abstract_ones_say_so() {
        // §9.3, §9.4: interface members are implicitly public and interface
        // fields are also static. I-06's candidate-order rule turns on exactly
        // the static bit, so it is not decoration.
        let f = facts(TYPES);
        let limit = def(&f, "LIMIT");
        assert!(limit.facets.contains(DefFacets::EXPORTED));
        assert!(limit.facets.contains(DefFacets::STATIC));
        let required = def(&f, "required");
        assert!(required.facets.contains(DefFacets::ABSTRACT));
        assert!(!def(&f, "provided").facets.contains(DefFacets::ABSTRACT));
        assert!(def(&f, "helper").facets.contains(DefFacets::STATIC));
    }

    #[test]
    fn enum_constants_are_fields_and_a_constant_body_declares_nothing() {
        // D-05, T-05: the constant is a nameable static field; its body is an
        // anonymous subclass (§8.9.3) and has no canonical name.
        let f = facts(TYPES);
        for name in ["RED", "GREEN"] {
            let d = def(&f, name);
            assert_eq!(d.kind, DefKind::Field);
            assert_eq!(d.owner, ["Color"]);
            assert!(d.facets.contains(DefFacets::STATIC));
        }
        assert_eq!(def(&f, "shared").owner, ["Color"]);
        assert!(
            defs_named(&f, "hidden").is_empty(),
            "an enum constant body is anonymous and declares nothing nameable"
        );
    }

    #[test]
    fn an_annotation_element_is_a_method() {
        // D-08 / §9.6.1.
        let f = facts(TYPES);
        let value = def(&f, "value");
        assert_eq!(value.kind, DefKind::Method);
        assert_eq!(value.owner, ["Anno"]);
        assert_eq!(value.params.as_ref().map(|p| p.count), Some(0));
        assert!(value.facets.contains(DefFacets::ABSTRACT));
    }

    #[test]
    fn local_and_anonymous_classes_are_not_definitions() {
        // T-03, T-04: §6.7 gives neither a canonical name, and §13.1's
        // `Outer$1` numbering is occurrence-ordered — using it as a NodeId
        // input would make inserting one anonymous class re-key every later
        // one, which is the whole-file ID cascade the identity decision was
        // made to avoid.
        let f = facts(
            "package p;

class A {
    void f() {
        class Local {
            void inner() {}
        }
        Runnable r = new Runnable() {
            public void run() {}
        };
    }
}
",
        );
        assert!(defs_named(&f, "Local").is_empty());
        assert!(defs_named(&f, "inner").is_empty());
        assert!(defs_named(&f, "run").is_empty());
        // What *is* declared: `A`, its default constructor, and `f`.
        let names: Vec<&str> = f
            .defs
            .iter()
            .filter(|d| d.kind != DefKind::Alias)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, ["p", "A", "A", "f"]);
    }

    #[test]
    fn the_enclosing_chain_is_a_path_that_skips_what_cannot_be_named() {
        // T-07, T-03, T-04, T-06: a call inside a lambda, a local class or an
        // anonymous class belongs to the nameable member around it, because
        // that is the only node an edge could start at.
        let f = facts(
            "package p;

class Outer {
    class Inner {
        void deep() {
            plain();
            Runnable r = () -> lambda();
            Runnable q = new Runnable() {
                public void run() {
                    anon();
                }
            };
            class Local {
                void lm() {
                    local();
                }
            }
        }
    }
}
",
        );
        let want = Some(Encloser {
            path: vec!["Outer".into(), "Inner".into(), "deep()".into()],
            kind: DefKind::Method,
        });
        for raw in ["plain", "lambda", "anon", "local"] {
            assert_eq!(site(&f, RefKind::Call, raw).enclosing, want, "`{raw}`");
        }
    }

    #[test]
    fn an_initializer_encloses_at_the_owning_type() {
        // D-11: a type is a node and JVMS §2.9.2's `<clinit>` is not a
        // nameable name, so a call in a field or static initializer is
        // attributed to the owning type.
        let f = facts(
            "package p;

class A {
    int n = compute();

    static {
        boot();
    }

    {
        setup();
    }
}
",
        );
        let want = Some(Encloser {
            path: vec!["A".into()],
            kind: DefKind::Type,
        });
        for raw in ["compute", "boot", "setup"] {
            assert_eq!(site(&f, RefKind::Call, raw).enclosing, want, "`{raw}`");
        }
        // A site above every type has no encloser at all and sources at the
        // file's container instead.
        let unit = facts("package p;\n\nimport q.R;\n\nclass A {}\n");
        assert_eq!(site(&unit, RefKind::Import, "q.R").enclosing, None);
    }

    // ---------------------------------------------------------------
    // Methods, overloads, arity (M-01, M-05, M-06, M-08, M-10)
    // ---------------------------------------------------------------

    #[test]
    fn overloads_are_separate_definitions_carrying_their_signature() {
        // M-01: `Foo#m(String)` and `Foo#m(int)` are two definitions and two
        // nodes. Collapsing them is silent graph corruption, not a lowered
        // rate — the most dangerous single gap on the case study's list.
        let f = facts(
            "package p;

class A {
    void m(String s) {}
    void m(int i) {}
    void m(String fmt, Object... args) {}
}
",
        );
        let shapes: Vec<(u32, bool, Vec<String>)> = defs_named(&f, "m")
            .iter()
            .filter_map(|d| d.params.as_ref())
            .map(|p| (p.count, p.varargs, p.types.clone()))
            .collect();
        assert_eq!(
            shapes,
            [
                (1, false, vec!["String".to_string()]),
                (1, false, vec!["int".to_string()]),
                // M-05 / §8.4.1: the `...` is kept because the declared type
                // *is* an array type, and dropping it would make `f(int)` and
                // `f(int...)` one key while §15.12.2 ranks them differently.
                (2, true, vec!["String".to_string(), "Object...".to_string()]),
            ]
        );
        let varargs = defs_named(&f, "m")
            .into_iter()
            .find(|d| d.params.as_ref().is_some_and(|p| p.varargs))
            .expect("a varargs overload");
        assert!(varargs.facets.contains(DefFacets::VARARGS));
    }

    #[test]
    fn the_enclosing_path_distinguishes_two_overloads() {
        // The core's `Encloser` carries no parameter shape, so the source of
        // an edge inside `m(String)` and one inside `m(int)` would be the same
        // node. Carrying the source-level parameter list in the last path
        // segment is the workaround; without it the graph is corrupt rather
        // than merely thin.
        let f = facts(
            "package p;

class A {
    void m(String s) { one(); }
    void m(int i) { two(); }
}
",
        );
        assert_eq!(
            site(&f, RefKind::Call, "one")
                .enclosing
                .as_ref()
                .map(|e| e.path.clone()),
            Some(vec!["A".to_string(), "m(String)".to_string()])
        );
        assert_eq!(
            site(&f, RefKind::Call, "two")
                .enclosing
                .as_ref()
                .map(|e| e.path.clone()),
            Some(vec!["A".to_string(), "m(int)".to_string()])
        );
    }

    #[test]
    fn every_call_and_creation_site_carries_an_argument_count() {
        // M-06: the one fact a call site has about the callee's signature that
        // its name does not carry, and the minimum for any overload
        // discrimination at all.
        let f = facts(&in_method(
            "        g();
        g(1);
        g(1, 2);
        new Foo();
        new Foo(1);",
        ));
        let calls: Vec<Option<u32>> = refs_of(&f, RefKind::Call).iter().map(|r| r.argc).collect();
        assert_eq!(calls, [Some(0), Some(1), Some(2)]);
        let news: Vec<Option<u32>> = refs_of(&f, RefKind::New).iter().map(|r| r.argc).collect();
        assert_eq!(news, [Some(0), Some(1)]);
        // A type use has no argument list, and `None` is a different fact from
        // `Some(0)`.
        assert_eq!(site(&f, RefKind::TypeUse, "Foo").argc, None);
    }

    #[test]
    fn explicit_type_arguments_do_not_split_the_dedup_key() {
        // M-08 / §15.12: type arguments constrain inference, not identity, so
        // the two sites below name one method and must be one row.
        let f = facts(&in_method(
            "        Collections.<String>emptyList();
        Collections.emptyList();",
        ));
        let raws: Vec<&str> = refs_of(&f, RefKind::Call)
            .iter()
            .map(|r| r.raw_target.as_str())
            .collect();
        assert_eq!(raws, ["Collections.emptyList", "Collections.emptyList"]);
        for r in refs_of(&f, RefKind::Call) {
            assert_eq!(segments(r), ["Collections", "emptyList"]);
        }
    }

    // ---------------------------------------------------------------
    // Implicit members (D-09, D-10, C-02)
    // ---------------------------------------------------------------

    #[test]
    fn a_record_synthesizes_its_components_accessors_and_canonical_constructor() {
        // D-09, D-10, C-02: `new Point(1, 2)`, `p.x()` and `p.equals(q)` all
        // name real members that no declaration syntax states. An extractor
        // that only reads declarations makes every one of them a false
        // `NoMatchingDefinition`.
        let f = facts(
            "package p;

record Point(int x, int y) {}
",
        );
        let synth: Vec<(&str, DefKind, u32)> = f
            .defs
            .iter()
            .filter(|d| d.kind != DefKind::Alias && d.facets.contains(DefFacets::SYNTHETIC))
            .map(|d| {
                (
                    d.name.as_str(),
                    d.kind,
                    d.params.as_ref().map_or(u32::MAX, |p| p.count),
                )
            })
            .collect();
        assert_eq!(
            synth,
            [
                ("x", DefKind::Field, u32::MAX),
                ("x", DefKind::Method, 0),
                ("y", DefKind::Field, u32::MAX),
                ("y", DefKind::Method, 0),
                ("Point", DefKind::Constructor, 2),
                ("equals", DefKind::Method, 1),
                ("hashCode", DefKind::Method, 0),
                ("toString", DefKind::Method, 0),
            ]
        );
        assert_eq!(
            defs_named(&f, "Point")
                .iter()
                .find(|d| d.kind == DefKind::Constructor)
                .and_then(|d| d.params.as_ref())
                .map(|p| p.types.clone()),
            Some(vec!["int".to_string(), "int".to_string()])
        );
    }

    #[test]
    fn a_written_member_is_not_synthesized_twice() {
        // §8.10.4: a compact constructor *is* the canonical one, and a written
        // accessor replaces the implicit one.
        let f = facts(
            "package p;

record Point(int x, int y) {
    Point {
        check();
    }

    public int x() {
        return x;
    }

    public String toString() {
        return \"\";
    }
}
",
        );
        let ctors = defs_named(&f, "Point");
        assert_eq!(
            ctors
                .iter()
                .filter(|d| d.kind == DefKind::Constructor)
                .count(),
            1,
            "the compact constructor is the canonical one"
        );
        // A compact constructor's parameters are the record's components.
        assert_eq!(
            ctors
                .iter()
                .find(|d| d.kind == DefKind::Constructor)
                .and_then(|d| d.params.as_ref())
                .map(|p| p.count),
            Some(2)
        );
        assert_eq!(
            defs_named(&f, "x")
                .iter()
                .filter(|d| d.kind == DefKind::Method)
                .count(),
            1
        );
        assert_eq!(defs_named(&f, "toString").len(), 1);
        assert!(!def(&f, "toString").facets.contains(DefFacets::SYNTHETIC));
    }

    #[test]
    fn an_enum_synthesizes_values_and_valueof() {
        // D-10 / §8.9.3.
        let f = facts(
            "package p;

enum Color { RED }
",
        );
        let values = def(&f, "values");
        assert!(values.facets.contains(DefFacets::STATIC));
        assert!(values.facets.contains(DefFacets::SYNTHETIC));
        assert_eq!(values.params.as_ref().map(|p| p.count), Some(0));
        assert_eq!(
            def(&f, "valueOf").params.as_ref().map(|p| p.types.clone()),
            Some(vec!["String".to_string()])
        );
    }

    #[test]
    fn a_class_with_no_constructor_gets_the_implicit_one() {
        // C-02 / §8.8.9: `new Foo()` on a source-constructorless class must
        // resolve, and it takes the class's own access.
        let f = facts(
            "package p;

public class A {}

class B {
    B(int x) {}
}
",
        );
        let a = defs_named(&f, "A");
        let ctor = a
            .iter()
            .find(|d| d.kind == DefKind::Constructor)
            .expect("a default constructor");
        assert!(ctor.facets.contains(DefFacets::SYNTHETIC));
        assert!(ctor.facets.contains(DefFacets::EXPORTED));
        assert_eq!(ctor.params.as_ref().map(|p| p.count), Some(0));
        // `B` writes one, so nothing is implied.
        assert_eq!(
            defs_named(&f, "B")
                .iter()
                .filter(|d| d.kind == DefKind::Constructor)
                .count(),
            1
        );
        assert!(
            !defs_named(&f, "B")
                .iter()
                .any(|d| d.facets.contains(DefFacets::SYNTHETIC))
        );
    }

    // ---------------------------------------------------------------
    // Inheritance, construction, and the target shapes (H-01, H-03, C-*)
    // ---------------------------------------------------------------

    #[test]
    fn supertypes_are_inherit_references_and_permits_is_not() {
        // H-01: member lookup cannot start until `extends`/`implements` have
        // themselves resolved, so these are load-bearing inputs and not
        // decoration. `permits` names *subtypes*, which is an ordinary type
        // use.
        let f = facts(
            "package p;

sealed class A extends q.Base<T> implements Iface, r.Other permits Sub {}
",
        );
        let inherits: Vec<Vec<&str>> = refs_of(&f, RefKind::Inherit)
            .iter()
            .map(|r| segments(r))
            .collect();
        assert_eq!(
            inherits,
            [vec!["q", "Base"], vec!["Iface"], vec!["r", "Other"]]
        );
        for r in refs_of(&f, RefKind::Inherit) {
            assert_eq!(r.space, DeclSpace::Type);
        }
        assert_eq!(site(&f, RefKind::TypeUse, "Sub").kind, RefKind::TypeUse);
        // The type argument inside `q.Base<T>` is still a type use of its own.
        assert_eq!(site(&f, RefKind::TypeUse, "T").kind, RefKind::TypeUse);
    }

    #[test]
    fn super_and_qualified_this_are_roots_and_not_expressions() {
        // H-03: `super.m()`, `Iface.super.m()` and `Outer.this.m()` are fully
        // resolvable with no inference anywhere. Letting them fall to an
        // expression root would throw away cheap wins under an
        // honest-sounding label.
        let f = facts(
            "package p;

class A {
    class Inner {
        void f() {
            super.m();
            Iface.super.m();
            Outer.this.m();
            this.m();
            this.field.m();
        }
    }
}
",
        );
        let by_raw = |raw: &str| site(&f, RefKind::Call, raw).target.clone();
        assert_eq!(
            by_raw("super.m"),
            RefTarget {
                root: TargetRoot::Super { qualifier: vec![] },
                segments: vec!["m".into()],
            }
        );
        assert_eq!(
            by_raw("Iface.super.m"),
            RefTarget {
                root: TargetRoot::Super {
                    qualifier: vec!["Iface".into()]
                },
                segments: vec!["m".into()],
            }
        );
        assert_eq!(
            by_raw("Outer.this.m"),
            RefTarget {
                root: TargetRoot::This {
                    qualifier: vec!["Outer".into()]
                },
                segments: vec!["m".into()],
            }
        );
        assert_eq!(
            by_raw("this.m"),
            RefTarget {
                root: TargetRoot::This { qualifier: vec![] },
                segments: vec!["m".into()],
            }
        );
        assert_eq!(
            by_raw("this.field.m"),
            RefTarget {
                root: TargetRoot::This { qualifier: vec![] },
                segments: vec!["field".into(), "m".into()],
            }
        );
    }

    #[test]
    fn creation_sites_name_a_constructor_and_a_type() {
        // C-01: two references in one expression. C-04: the enclosing instance
        // is not part of the target, so `outer.new Inner()` and
        // `new Outer.Inner()` normalise to the same shape of name. C-06: a
        // diamond still names the raw type, because erasure discards the
        // arguments anyway.
        let f = facts(&in_method(
            "        new Foo(1);
        new a.b.Foo<>();
        outer.new Inner();
        new Outer.Inner();",
        ));
        let news: Vec<(Vec<&str>, Option<u32>)> = refs_of(&f, RefKind::New)
            .iter()
            .map(|r| (segments(r), r.argc))
            .collect();
        assert_eq!(
            news,
            [
                (vec!["Foo"], Some(1)),
                (vec!["a", "b", "Foo"], Some(0)),
                (vec!["Inner"], Some(0)),
                (vec!["Outer", "Inner"], Some(0)),
            ]
        );
        // The type half of `new Foo(1)` is a type use of its own.
        assert!(
            refs_of(&f, RefKind::TypeUse)
                .iter()
                .any(|r| segments(r) == ["Foo"])
        );
    }

    #[test]
    fn array_creation_names_no_constructor() {
        // C-07 / §15.10.1: `new int[10]` is not a reference to any definition,
        // and inventing one would put a guess in the denominator.
        let f = facts(&in_method(
            "        int[] a = new int[10];
        String[] b = new String[] { \"x\" };
        Foo c = new Foo();",
        ));
        let news: Vec<Vec<&str>> = refs_of(&f, RefKind::New)
            .iter()
            .map(|r| segments(r))
            .collect();
        assert_eq!(news, [vec!["Foo"]]);
    }

    #[test]
    fn an_anonymous_creation_targets_the_named_supertype() {
        // C-05 / §15.9.5.1: the constructor actually invoked is the
        // supertype's — the anonymous class has none that can be named
        // (T-04) — so the target is the type that was written.
        let f = facts(&in_method(
            "        Runnable r = new Runnable() {
            public void run() {}
        };",
        ));
        let news = refs_of(&f, RefKind::New);
        assert_eq!(news.len(), 1);
        assert_eq!(segments(news[0]), ["Runnable"]);
        assert_eq!(news[0].argc, Some(0));
    }

    #[test]
    fn this_and_super_constructor_invocations_are_creation_sites() {
        // C-03 / §8.8.7.1: both name `<init>` on a type the file knows, and
        // both are fully resolvable.
        let f = facts(
            "package p;

class A {
    A() {
        this(1);
    }

    A(int x) {
        super(x);
    }
}
",
        );
        let news: Vec<(&TargetRoot, Option<u32>)> = refs_of(&f, RefKind::New)
            .iter()
            .map(|r| (&r.target.root, r.argc))
            .collect();
        assert_eq!(
            news,
            [
                (&TargetRoot::This { qualifier: vec![] }, Some(1)),
                (&TargetRoot::Super { qualifier: vec![] }, Some(1)),
            ]
        );
        for r in refs_of(&f, RefKind::New) {
            assert!(r.target.segments.is_empty());
            assert!(!r.locally_bound);
        }
    }

    #[test]
    fn an_enum_constant_with_arguments_is_a_creation_site() {
        // §8.9.1. A constant with no argument list has no written site, and
        // C-09 forbids inventing one.
        let f = facts(
            "package p;

enum Color {
    RED(1),
    GREEN;

    Color() {}

    Color(int n) {}
}
",
        );
        let news: Vec<(Vec<&str>, Option<u32>)> = refs_of(&f, RefKind::New)
            .iter()
            .map(|r| (segments(r), r.argc))
            .collect();
        assert_eq!(news, [(vec!["Color"], Some(1))]);
    }

    #[test]
    fn method_references_name_a_method_or_a_constructor() {
        // C-08 / §15.13. `<init>` cannot be a Java identifier (§3.8 excludes
        // `<` and `>`), so it names the constructor unambiguously. The arity
        // is `None` because the overload is chosen by the target functional
        // interface type, not by an argument list at this site.
        let f = facts(&in_method(
            "        Supplier<Foo> a = Foo::new;
        Runnable b = Outer::helper;
        Function<K, V> c = a.b.C::get;
        Runnable d = local::run;",
        ));
        let shapes: Vec<(Vec<&str>, Option<u32>)> = refs_of(&f, RefKind::MethodRef)
            .iter()
            .map(|r| (segments(r), r.argc))
            .collect();
        assert_eq!(
            shapes,
            [
                (vec!["Foo", "<init>"], None),
                (vec!["Outer", "helper"], None),
                (vec!["a", "b", "C", "get"], None),
                (vec!["local", "run"], None),
            ]
        );
    }

    // ---------------------------------------------------------------
    // Qualifiers and honest limits (N-04, X-01)
    // ---------------------------------------------------------------

    #[test]
    fn a_multi_segment_qualifier_keeps_every_segment() {
        // N-04: `RefTarget` carrying one qualifier identifier would collapse
        // every `org.slf4j.LoggerFactory.getLogger(…)`-shaped call into an
        // expression root and hide a fully resolvable case behind
        // `NeedsTypeInference`. §6.5.2's reclassification needs the segments.
        let f = facts(&in_method(
            "        org.slf4j.LoggerFactory.getLogger(A.class);
        p.q();
        r();",
        ));
        assert_eq!(
            segments(site(&f, RefKind::Call, "org.slf4j.LoggerFactory.getLogger")),
            ["org", "slf4j", "LoggerFactory", "getLogger"]
        );
        assert_eq!(segments(site(&f, RefKind::Call, "p.q")), ["p", "q"]);
        assert_eq!(segments(site(&f, RefKind::Call, "r")), ["r"]);
        for raw in ["org.slf4j.LoggerFactory.getLogger", "p.q", "r"] {
            assert_eq!(site(&f, RefKind::Call, raw).target.root, TargetRoot::Name);
        }
    }

    #[test]
    fn an_expression_receiver_keeps_only_its_trailing_selectors() {
        // X-01 / §15.12.1: the type of a Primary is genuinely needed, and this
        // is the one call shape the study says to leave alone.
        let f = facts(&in_method(
            "        getService().start();
        list.get(0).foo();
        ((Foo) x).m();
        arr[i].m();",
        ));
        let expressions: Vec<(&str, Vec<&str>)> = refs_of(&f, RefKind::Call)
            .iter()
            .filter(|r| r.target.root == TargetRoot::Expr)
            .map(|r| (r.raw_target.as_str(), segments(r)))
            .collect();
        assert_eq!(
            expressions,
            [
                ("getService().start", vec!["start"]),
                ("list.get(0).foo", vec!["foo"]),
                ("((Foo)x).m", vec!["m"]),
                ("arr[i].m", vec!["m"]),
            ]
        );
    }

    #[test]
    fn a_field_access_is_a_site_only_where_it_is_not_a_qualifier() {
        // The chain's segments already carry the qualifier, so emitting the
        // inner accesses too would count one name several times and inflate
        // the denominator.
        let f = facts(&in_method(
            "        System.out.println(x);
        a.b.c = 1;
        int n = Integer.MAX_VALUE;",
        ));
        let accesses: Vec<Vec<&str>> = refs_of(&f, RefKind::FieldAccess)
            .iter()
            .map(|r| segments(r))
            .collect();
        assert_eq!(
            accesses,
            [vec!["a", "b", "c"], vec!["Integer", "MAX_VALUE"]]
        );
        assert_eq!(
            segments(site(&f, RefKind::Call, "System.out.println")),
            ["System", "out", "println"]
        );
    }

    #[test]
    fn annotations_are_references_to_a_type() {
        let f = facts(
            "package p;

@Service
@q.Scoped(\"x\")
class A {
    @Override
    public String toString() {
        return \"\";
    }
}
",
        );
        let annotations: Vec<Vec<&str>> = refs_of(&f, RefKind::Annotation)
            .iter()
            .map(|r| segments(r))
            .collect();
        assert_eq!(
            annotations,
            [vec!["Service"], vec!["q", "Scoped"], vec!["Override"]]
        );
        for r in refs_of(&f, RefKind::Annotation) {
            assert_eq!(r.space, DeclSpace::Type);
        }
    }
}
