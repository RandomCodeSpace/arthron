//! Go extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/go.yml`) select nodes by kind; this
//! module interprets their fields. Only package-level declarations become
//! definitions — locals are not nodes.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::resolve_go::GoLang;
use crate::sg::{Rules, SgNode, SourceTree, span_of};

/// The embedded Go extraction rules.
const GO_RULES: &str = include_str!("rules/go.yml");

/// One `import` spec: optional alias (`.` and `_` included) and the
/// unquoted import path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// Alias if written (`f "fmt"` → `f`; `. "x"` → `.`; `_ "x"` → `_`).
    pub alias: Option<String>,
    /// The import path with surrounding quotes stripped, whether the
    /// literal was interpreted (`"fmt"`) or raw (`` `fmt` ``).
    pub path: String,
    /// Where the spec sits.
    pub span: Span,
}

/// Per-file Go facts only the Go resolver reads.
///
/// `rel_path` is here because a Go file's package path is a fact about
/// *where the file is*, and [`Extractor::extract`] is handed the path for
/// exactly this: the resolver needs it, and the core must not be the layer
/// that turns a path into a package.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoHeader {
    /// Repo-relative, `/`-separated path of the file.
    pub rel_path: String,
    /// The declared package name, if the file parsed far enough to have one.
    pub package: Option<String>,
    /// All import specs, in source order.
    ///
    /// Every one of these is *also* a [`RefKind::Import`] reference in
    /// `refs`: the reference is the extractor's, the binding effect it has
    /// on the file's scope is the resolver's.
    pub imports: Vec<Import>,
    /// Byte offsets of the composite-literal keys reported in `refs`.
    ///
    /// A literal key and a selector become the same reference: both are a
    /// [`RefKind::FieldAccess`] carrying `[Owner, Member]`, and nothing in
    /// the reference says which syntax produced it. They are not the same
    /// question. `T.M` written as a selector is Go's method expression and
    /// names the method; a literal key never can, because Go forbids a type
    /// from declaring a method and a field of one name, so a literal key that
    /// *did* find `T.M` would have found the wrong node. Only the extractor
    /// saw the syntax, so it says so here — the one file-local fact the
    /// resolver cannot re-derive.
    ///
    /// A byte offset identifies the site: a key is one identifier, and no
    /// selector can begin at a key identifier's first byte, because a key is
    /// followed by `:`.
    pub literal_keys: Vec<u32>,
}

/// The Go extractor. Stateless.
pub struct GoExtractor;

impl Extractor<GoLang> for GoExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<GoLang> {
        extract(rel_path, source)
    }
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules::compile(GO_RULES).expect("embedded go.yml compiles"))
}

/// True when the node sits inside any function body.
fn inside_function(node: &SgNode) -> bool {
    node.ancestors().any(|a| {
        matches!(
            &*a.kind(),
            "function_declaration" | "method_declaration" | "func_literal"
        )
    })
}

/// The first `type_identifier` in a subtree — the receiver's type name.
fn receiver_type_name(receiver: &SgNode) -> Option<String> {
    receiver
        .dfs()
        .find(|n| n.kind() == "type_identifier")
        .map(|n| n.text().to_string())
}

/// The nearest *nameable* enclosing definition.
///
/// Function literals are skipped: an anonymous function is not a node, so a
/// call inside one belongs to the named definition around it.
fn enclosing_definition(node: &SgNode) -> Option<Encloser> {
    for a in node.ancestors() {
        match &*a.kind() {
            "function_declaration" => {
                let name = a.field("name")?.text().to_string();
                return Some(Encloser {
                    path: vec![name],
                    kind: DefKind::Function,
                });
            }
            "method_declaration" => {
                let name = a.field("name")?.text().to_string();
                let recv = a
                    .field("receiver")
                    .and_then(|r| receiver_type_name(&r))
                    .unwrap_or_default();
                return Some(Encloser {
                    path: vec![recv, name],
                    kind: DefKind::Method,
                });
            }
            _ => {}
        }
    }
    None
}

/// Parse the `function` field of a `call_expression` into a target shape.
///
/// The selector chain is walked to its innermost operand: an identifier
/// there makes the whole dotted path a [`TargetRoot::Name`] target, and
/// anything else makes it [`TargetRoot::Expr`] carrying only the trailing
/// selectors. The *number* of segments is what the resolver dispatches on,
/// so a three-deep chain stays distinguishable from a qualified name
/// instead of collapsing into one "complex" bucket.
fn call_target(function: &SgNode) -> RefTarget {
    let mut segments: Vec<String> = Vec::new();
    let mut node = function.clone();
    loop {
        match &*node.kind() {
            "identifier" => {
                segments.push(node.text().to_string());
                segments.reverse();
                return RefTarget {
                    root: TargetRoot::Name,
                    segments,
                };
            }
            "selector_expression" => {
                let (Some(operand), Some(field)) = (node.field("operand"), node.field("field"))
                else {
                    break;
                };
                segments.push(field.text().to_string());
                node = operand;
            }
            _ => break,
        }
    }
    segments.reverse();
    RefTarget {
        root: TargetRoot::Expr,
        segments,
    }
}

/// Whether this type-use site stands in the *callee* position of a call.
///
/// `Generic[MyInt](1)` is a call, and the grammar disagrees: an explicit
/// instantiation is unambiguously a type, so tree-sitter parses the whole
/// thing as a `type_conversion_expression` over a `generic_type` and
/// `ref-call` — which matches `call_expression` — never sees it. The site
/// then produced one row, a `TypeUse` naming a *function*.
///
/// `src/rules/go.yml` states the rule this restores: whatever stands in the
/// callee position of a call written in call syntax is a `Call`, which is
/// already how the bare-identifier conversion `MyInt(3)` is reported. One
/// row either way; this only stops it lying about which kind it is.
///
/// The type *arguments* are untouched — `MyInt` is a written type name and
/// still its own reference.
fn instantiated_callee(site: &SgNode) -> bool {
    let Some(generic) = site.parent() else {
        return false;
    };
    if generic.kind() != "generic_type"
        || generic
            .children()
            .next()
            .is_none_or(|head| head.range() != site.range())
    {
        return false;
    }
    generic.parent().is_some_and(|call| {
        call.kind() == "type_conversion_expression"
            && call
                .children()
                .find(SgNode::is_named)
                .is_some_and(|t| t.range() == generic.range())
    })
}

/// Re-root a receiver-rooted target at [`TargetRoot::This`].
///
/// `t.M()` inside `func (t *T) …` is `this.M()`: the leading segment names
/// the receiver, so it is dropped and what remains is the member path. `Err`
/// carries the target back unchanged when nothing is selected through the
/// receiver — a bare `t()`, which names the receiver value and not a member.
fn receiver_target(target: RefTarget) -> Result<RefTarget, RefTarget> {
    if target.segments.len() < 2 {
        return Err(target);
    }
    Ok(RefTarget {
        root: TargetRoot::This { qualifier: vec![] },
        segments: target.segments[1..].to_vec(),
    })
}

/// Whether an `expression_list` of declaration targets names `name`.
fn list_binds(list: &SgNode, name: &str) -> bool {
    list.children()
        .any(|c| c.kind() == "identifier" && c.text() == name)
}

/// Whether a `:=` binder — a short variable declaration, a range clause, a
/// receive statement — declares `name` on its left.
fn declares(binder: &SgNode, name: &str) -> bool {
    binder.field("left").is_some_and(|l| list_binds(&l, name))
}

/// Whether a node's direct children include the short-declaration token.
///
/// `for k = range m` and `case v = <-ch:` assign to names bound elsewhere;
/// only `:=` declares. Reading an assignment as a binder would move a real
/// edge into the local bucket, which raises the rate by deleting a reference
/// from both of its terms.
fn short_declares(node: &SgNode) -> bool {
    node.children().any(|c| c.kind() == ":=")
}

/// Whether a `var_spec` / `const_spec` / `type_spec` / `type_alias` declares
/// `name`.
fn spec_binds(spec: &SgNode, name: &str) -> bool {
    let is_it =
        |n: &SgNode| matches!(&*n.kind(), "identifier" | "type_identifier") && n.text() == name;
    spec.field_children("name").any(|n| is_it(&n)) || spec.field("name").is_some_and(|n| is_it(&n))
}

/// The spec kinds a `type_declaration` is built from. `type_alias` does not
/// end in `_spec`, and a generic alias declares a name like any other.
fn is_type_spec(kind: &str) -> bool {
    matches!(kind, "type_spec" | "type_alias")
}

/// Whether a statement in a block or case clause declares `name` visibly at
/// `site`.
///
/// Go gives values and types *different* starting points, and the difference
/// is observable. "The scope of a constant or variable identifier declared
/// inside a function begins at the end of the ConstSpec or VarSpec" — so
/// `x := x()` names the outer `x`. "The scope of a type identifier declared
/// inside a function begins at the identifier in the TypeSpec" — so
/// `type ring struct{ next *ring }` names *itself*, and reading the value rule
/// here would report that field as a package-level type that does not exist.
fn statement_binds(stmt: &SgNode, name: &str, site: usize) -> bool {
    match &*stmt.kind() {
        "short_var_declaration" => stmt.range().end <= site && declares(stmt, name),
        "var_declaration" | "const_declaration" => {
            stmt.range().end <= site
                && stmt
                    .children()
                    .any(|spec| spec.kind().ends_with("_spec") && spec_binds(&spec, name))
        }
        "type_declaration" => stmt.children().any(|spec| {
            is_type_spec(&spec.kind())
                && spec_binds(&spec, name)
                && spec.field("name").is_some_and(|n| n.range().start <= site)
        }),
        _ => false,
    }
}

/// Whether a clause's header binds `name` at a site starting at `site`.
///
/// A declared identifier's scope starts at the *end* of its declaration, so a
/// header binds the part of its clause that follows it and not its own
/// right-hand side: in `if x := x(); cond`, the `x()` names whatever `x` was
/// in scope before the header — typically a package-level function or an
/// import — and only the body sees the new one. Binding the initialiser too
/// would move a real reference into the local bucket, deleting it from
/// *both* terms of the resolution rate.
fn header_binds(clause: &SgNode, name: &str, site: usize) -> bool {
    let init_binds = |c: &SgNode| {
        c.kind() == "short_var_declaration" && declares(c, name) && c.range().end <= site
    };
    match &*clause.kind() {
        // `if v := f(); cond` is visible in the consequence *and* the else;
        // `switch v := f(); x` in every case. In `f()` itself, never.
        "if_statement" | "expression_switch_statement" => clause.children().any(|c| init_binds(&c)),
        // `switch v := x.(type)` binds `v` in every case clause. The alias
        // is a bare `expression_list` before `:=`, not a declaration node,
        // so it is read positionally, and the guard it closes runs to the
        // body's opening brace — `switch v := v.(type)` is legal and its
        // operand is the outer `v`. An optional initialiser binds like an
        // ordinary switch's. With no brace the file did not parse that far;
        // not binding keeps the reference in the rate rather than deleting
        // it.
        "type_switch_statement" => {
            clause.children().any(|c| init_binds(&c))
                || (short_declares(clause)
                    && clause
                        .children()
                        .any(|c| c.kind() == "expression_list" && list_binds(&c, name))
                    && clause
                        .children()
                        .find(|c| c.kind() == "{")
                        .is_some_and(|body| body.range().start <= site))
        }
        "for_statement" => clause.children().any(|c| match &*c.kind() {
            "for_clause" => c.children().any(|i| init_binds(&i)),
            "range_clause" => short_declares(&c) && declares(&c, name) && c.range().end <= site,
            _ => false,
        }),
        // `case v := <-ch:` binds `v` in that clause alone, and not in the
        // channel expression it receives from.
        "communication_case" => clause.children().any(|c| {
            c.kind() == "receive_statement"
                && short_declares(&c)
                && declares(&c, name)
                && c.range().end <= site
        }),
        _ => false,
    }
}

/// Whether a function-like node's signature binds `name`.
///
/// Parameters, named results and the receiver, which are one shape in the Go
/// grammar. Only the *direct* parameter lists are read: a `func(inner int)`
/// parameter type carries a list of its own, and the names in it belong to
/// that type rather than to this body.
///
/// Which of those lists matched is not recorded here — [`binder_of`] asks
/// [`receiver_binds`] separately, because a receiver is not an ordinary local.
fn signature_binds(func: &SgNode, name: &str) -> bool {
    for list in func.children().filter(|c| c.kind() == "parameter_list") {
        for decl in list.children() {
            let declaring = matches!(
                &*decl.kind(),
                "parameter_declaration" | "variadic_parameter_declaration"
            );
            if declaring
                && decl
                    .children()
                    .any(|n| n.kind() == "identifier" && n.text() == name)
            {
                return true;
            }
        }
    }
    false
}

/// Whether a method's *receiver* names `name`.
///
/// Only the `receiver` parameter list, which is what separates Go's spelling
/// of `this` from an ordinary parameter. Go has no `this` keyword: the
/// receiver *is* the name a method uses to reach its own value, and its type
/// is written in the signature — the strongest declared-type evidence any
/// tier-1 language gives. So a member selected through it is not a local
/// binding; see [`Binder`].
fn receiver_binds(func: &SgNode, name: &str) -> bool {
    let Some(receiver) = func.field("receiver") else {
        return false;
    };
    receiver.children().any(|decl| {
        matches!(
            &*decl.kind(),
            "parameter_declaration" | "variadic_parameter_declaration"
        ) && decl
            .children()
            .any(|n| n.kind() == "identifier" && n.text() == name)
    })
}

/// Whether a node's own `type_parameter_list` declares `name`.
///
/// Hung off functions, methods *and* type declarations, which is why this is
/// separate from [`signature_binds`]: `type Box[T any] struct{ v T }` binds
/// `T` in its own body at package level, where there is no enclosing function
/// for the block walk to gate on. A type parameter's name is an `identifier`
/// in this grammar, not a `type_identifier` — the constraint beside it is the
/// `type_identifier`, and that one really is a reference.
fn type_parameters_bind(node: &SgNode, name: &str) -> bool {
    node.children()
        .filter(|c| c.kind() == "type_parameter_list")
        .any(|list| {
            list.children()
                .filter(|d| d.kind() == "type_parameter_declaration")
                .any(|d| {
                    d.children()
                        .any(|n| n.kind() == "identifier" && n.text() == name)
                })
        })
}

/// Whether a method's *receiver* re-declares `name` as a type parameter.
///
/// `func (b *Box[T]) Get() T` writes `T` twice, and only the second is a
/// reference: the receiver's type arguments declare the method's type
/// parameters. Without this the result `T` would be reported
/// `NoMatchingDefinition` — the bucket reserved for arthron's own bugs.
fn receiver_type_parameters_bind(func: &SgNode, name: &str) -> bool {
    let Some(receiver) = func.field("receiver") else {
        return false;
    };
    receiver.dfs().any(|n| {
        n.kind() == "type_arguments"
            && n.dfs()
                .any(|t| t.kind() == "type_identifier" && t.text() == name)
    })
}

/// What binds `name` at a site, when anything in this file does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binder {
    /// Nothing inside a nameable definition binds it: package level answers.
    None,
    /// A parameter, named result, type parameter, or a name some enclosing
    /// block declares. Not a node by design; see
    /// [`crate::UnresolvedReason::LocalBinding`].
    Local,
    /// The enclosing method's *receiver*, and no inner binder shadows it.
    ///
    /// Go's spelling of `this`. Every other tier-1 language writes this shape
    /// as `this.m()` or `self.m()` and resolves it by declared-type lookup;
    /// so does Go, from the type its own signature states.
    Receiver,
}

/// Which enclosing binder in this file binds `name` at this site.
///
/// A *file-local verdict*, and the whole of it: every Go binder for a value
/// name is decidable from one file's AST, which is why nothing richer than
/// this crosses the extractor/resolver boundary. The extractor states the
/// fact; the resolver still owns the outcome.
///
/// Two rules are not optional. A declared identifier's scope starts at the
/// end of its declaration, so a binder inside a block is visible only when
/// it closes before the site — parameters, named results and receivers are
/// exempt, binding the whole body. And package level is not a binding
/// environment: with no function, method or literal above it, a reference
/// can never name a local.
///
/// The innermost binder wins, which is why `bound` is read *before* the
/// signature: `func (t *T) M() { t := other(); t.n() }` names the block's
/// `t`, not the receiver.
fn binder_of(node: &SgNode, name: &str) -> Binder {
    if name == "_" {
        return Binder::None; // the blank identifier declares nothing
    }
    let site = node.range().start;
    let mut bound = false;
    let mut in_function = false;
    for ancestor in node.ancestors() {
        match &*ancestor.kind() {
            "function_declaration" | "method_declaration" | "func_literal" => {
                if bound {
                    return Binder::Local; // an inner block shadows the signature
                }
                if receiver_binds(&ancestor, name) {
                    return Binder::Receiver;
                }
                if signature_binds(&ancestor, name)
                    || type_parameters_bind(&ancestor, name)
                    || receiver_type_parameters_bind(&ancestor, name)
                {
                    return Binder::Local;
                }
                in_function = true;
            }
            // A generic type declaration is the one binder that is *not*
            // inside a function: `type Box[T any] struct{ v T }` binds `T`
            // over its own body at package level, so the `in_function` gate
            // below would never let it through.
            "type_spec" | "type_alias" => {
                if type_parameters_bind(&ancestor, name) {
                    return Binder::Local;
                }
            }
            // Statements of a block, a case clause or a select clause. The
            // position rule lives in `statement_binds`, because values and
            // types do not share one.
            "statement_list" => {
                bound = bound || ancestor.children().any(|s| statement_binds(&s, name, site));
            }
            _ => bound = bound || header_binds(&ancestor, name, site),
        }
        if bound && in_function {
            return Binder::Local;
        }
    }
    Binder::None
}

/// Whether some enclosing binder in this file makes `name` a non-node.
///
/// The type-position answer, and the receiver counts here: Go declares types
/// and values in one namespace, so `func (T *T) m()` really does shadow the
/// package-level type `T` inside that body, and a `type_identifier` reaching
/// the receiver is naming something no longer nameable from outside. A value
/// position asks [`binder_of`] instead, because there the receiver is `this`.
fn is_locally_bound(node: &SgNode, name: &str) -> bool {
    binder_of(node, name) != Binder::None
}

/// Whether a `type_identifier` *declares* the name it writes rather than
/// naming one.
///
/// Two positions do. A `type_spec` or `type_alias` `name` field is the
/// declaration itself — and only the `name` field, because `type A = B`
/// writes both halves as `type_identifier` and dropping `B` would delete a
/// real reference. And a method receiver's type arguments re-declare the
/// receiver type's parameters: in `func (b *Box[R]) Get() R` the first `R`
/// binds and the second names.
fn declares_its_own_name(node: &SgNode) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if is_type_spec(&parent.kind())
        && parent
            .field("name")
            .is_some_and(|n| n.range() == node.range())
    {
        return true;
    }
    let mut in_type_arguments = false;
    for a in node.ancestors() {
        match &*a.kind() {
            "type_arguments" => in_type_arguments = true,
            "parameter_list" => {
                return in_type_arguments
                    && a.parent().is_some_and(|p| {
                        p.kind() == "method_declaration"
                            && p.field("receiver").is_some_and(|r| r.range() == a.range())
                    });
            }
            _ => {}
        }
    }
    false
}

/// The site a `type_identifier` reference is written at, and its shape.
///
/// A `qualified_type` writes its package as a `package_identifier` and its
/// member as the `type_identifier` this is called on, so the *qualified node*
/// is the site: one reference for `pkg.T`, two segments, and the same shape a
/// two-segment call already has. Everything else is one segment.
fn type_use_site<'r>(node: &SgNode<'r>) -> (SgNode<'r>, RefTarget) {
    if let Some(parent) = node.parent()
        && parent.kind() == "qualified_type"
        && let Some(package) = parent.field("package")
    {
        let target = RefTarget {
            root: TargetRoot::Name,
            segments: vec![package.text().to_string(), node.text().to_string()],
        };
        return (parent, target);
    }
    let target = RefTarget {
        root: TargetRoot::Name,
        segments: vec![node.text().to_string()],
    };
    (node.clone(), target)
}

/// Whether a `selector_expression` is a *read* site of its own.
///
/// Two selectors are not: the one standing in a call's callee position, which
/// `ref-call` already reports as the call it is, and the one standing as
/// another selector's operand, because `a.b.c` names `c` once through `a.b`
/// and reporting the inner `a.b` too would invent a reference to a member
/// nobody named on its own. That is the same rule Java's `field_access` reads
/// (N-04): the outermost node of a chain carries the whole path.
fn is_read_site(node: &SgNode) -> bool {
    match node.parent() {
        Some(parent) => match &*parent.kind() {
            "selector_expression" => parent
                .field("operand")
                .is_none_or(|operand| operand.range() != node.range()),
            "call_expression" => parent
                .field("function")
                .is_none_or(|function| function.range() != node.range()),
            _ => true,
        },
        None => true,
    }
}

/// Apply the root-binding rule to a target read at `site`.
///
/// Only the *root* of the chain can be bound: `x.y.z` with `x` a parameter
/// names a local however long the member path is, which is why the shape
/// carries a root rather than a `Local` variant. A receiver root is re-rooted
/// at [`TargetRoot::This`] instead — Go's spelling of `this.m` — and a *bare*
/// receiver is the one case that stays local, because naming the receiver
/// value alone names something no more a node than a parameter is.
///
/// One function because a call and a read must not disagree about it: they
/// are the same question asked at two grammar positions, and Go's own rate is
/// compared against languages that ask it once.
fn bind_target(site: &SgNode, target: RefTarget) -> (RefTarget, bool) {
    match (&target.root, target.segments.first()) {
        (TargetRoot::Name, Some(root)) => match binder_of(site, root) {
            Binder::None => (target, false),
            Binder::Local => (target, true),
            Binder::Receiver => match receiver_target(target) {
                Ok(this) => (this, false),
                Err(bare) => (bare, true),
            },
        },
        _ => (target, false),
    }
}

/// Strip the wrappers that stand between a written type and its name.
///
/// `*T`, `(T)` and `T[A]` all name `T`; a composite literal's element type is
/// written with any of them. A `generic_type`'s first named child is its own
/// head, which is the name — its arguments are separate references that
/// `ref-type` already reports.
fn peel_type<'r>(node: &SgNode<'r>) -> SgNode<'r> {
    match &*node.kind() {
        "pointer_type" | "parenthesized_type" => node
            .children()
            .find(SgNode::is_named)
            .map_or_else(|| node.clone(), |inner| peel_type(&inner)),
        "generic_type" => node
            .children()
            .find(SgNode::is_named)
            .unwrap_or_else(|| node.clone()),
        _ => node.clone(),
    }
}

/// The type a `literal_value` is a literal *of*, following elided nesting.
///
/// A composite literal states its type once: `T{…}` on the literal itself,
/// `[]T{{…}}` and `map[string]T{"k": {…}}` on the container, with the element
/// literal writing no type at all. Go allows the elision at any depth, so this
/// climbs to the enclosing `literal_value` and takes that container's element
/// type — the `element` of a slice or array, the `value` of a map. A struct
/// container has no element type and no elision to follow: the spec allows a
/// nested literal to omit its type only "within a composite literal of array,
/// slice, or map type T", so a struct field's value always writes its own and
/// a struct container is never what an elided literal elided against. The
/// arm answers `None` because there is nothing to answer, not because a
/// lookup is missing; it is unreachable in Go that compiles, and in a tree
/// that does not, `None` leaves the key unreported rather than attributed to
/// the wrong type.
fn literal_type<'r>(literal_value: &SgNode<'r>) -> Option<SgNode<'r>> {
    let parent = literal_value.parent()?;
    if parent.kind() == "composite_literal" {
        return parent.field("type").as_ref().map(peel_type);
    }
    let outer = literal_value
        .ancestors()
        .find(|a| a.kind() == "literal_value")?;
    let container = literal_type(&outer)?;
    let element = match &*container.kind() {
        "slice_type" | "array_type" | "implicit_length_array_type" => container.field("element"),
        "map_type" => container.field("value"),
        _ => None,
    }?;
    Some(peel_type(&element))
}

/// The dotted path of a *named* type node, or `None` when it has no name.
///
/// `T` is one segment and `pkg.T` is two — the same shape [`type_use_site`]
/// gives a written type name, because it is the same path. Everything else —
/// a map, a slice, an anonymous `struct{…}`, an interface — names no node, so
/// there is nothing for a member of it to be a member of.
///
/// It answers what the name *is*, never what it was declared as, and a
/// file-local extractor has no second question to ask: `type Registry
/// map[Key]int` is usually written in another file of the same package. So a
/// *named* map, slice or array type reaches [`key_identifier`] exactly as a
/// named struct does, and its key — an index expression, not a member name —
/// is reported as `T.Key`. That is a wrong [`RefKind`], a wrong `raw_target`
/// and a row in `NeedsReceiverType` that should not exist: 9 rows / 90
/// occurrences on `codeiq` (`CapabilityMatrix`, declared in `plan.go` and
/// built in `capabilities.go`) and 0 on `caddy`, all of it inside the
/// denominator, so it understates the rate rather than flattering it —
/// 69.5% as measured against 70.0% without those rows.
///
/// Restricting emission to what one file can prove is not the fix. The file
/// declares the literal's type at only 249 of `codeiq`'s 643 unqualified
/// literal-key sites and 519 of `caddy`'s 2,482, so the restriction would
/// take 55% of `codeiq`'s and 79% of `caddy`'s legitimate struct keys with
/// it, and every `pkg.T{…}` besides — a denominator that depends on which
/// file a type was written in is a worse defect than the one it fixes. The
/// resolver does see the whole package, but no outcome it can return means
/// "this site is not a reference", and inventing one is not a thing this
/// track does. Closing it needs the extractor/resolver contract to change,
/// which is a design decision and not this function's.
///
/// What *is* closed here is the harm: `GoResolver::resolve_field` never
/// probes a literal key's member, so no such site can link.
fn named_type_path(node: &SgNode) -> Option<Vec<String>> {
    match &*node.kind() {
        "type_identifier" => Some(vec![node.text().to_string()]),
        "qualified_type" => {
            let package = node.field("package")?;
            let name = node.field("name")?;
            Some(vec![package.text().to_string(), name.text().to_string()])
        }
        _ => None,
    }
}

/// The identifier a `keyed_element` writes as its key, when it writes one.
///
/// A key is a `literal_element` wrapping one expression. Only a bare
/// identifier can be a field name; a string, an integer or anything compound
/// is a map key or an array index, which is an expression rather than a member
/// name.
fn key_identifier<'r>(keyed: &SgNode<'r>) -> Option<SgNode<'r>> {
    let key = keyed.field("key")?;
    let inner = key.children().find(SgNode::is_named)?;
    (inner.kind() == "identifier").then_some(inner)
}

/// The number of arguments at a call site.
///
/// A spread (`f(a, b...)`) counts as one argument: Go does not discriminate
/// by arity, so this is a dedup key component rather than a resolution
/// input. `Some(0)` and `None` are different facts and stay different keys.
fn argument_count(call: &SgNode) -> Option<u32> {
    let list = call.field("arguments")?;
    let count = list
        .children()
        .filter(|c| c.is_named() && c.kind() != "comment")
        .count();
    u32::try_from(count).ok()
}

/// A Go definition. Go declares everything in one space and carries no
/// facets or arity, so only the varying fields are parameters.
fn go_def(kind: DefKind, name: String, owner: Vec<String>, span: Span) -> Definition {
    Definition {
        kind,
        name,
        owner,
        space: DeclSpace::Value,
        facets: DefFacets::default(),
        params: None,
        span,
    }
}

/// Extract all facts from one Go source file.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<GoLang> {
    let tree = SourceTree::parse_go(source);
    let mut header = GoHeader {
        rel_path: rel_path.to_string(),
        package: None,
        imports: Vec::new(),
        literal_keys: Vec::new(),
    };
    let mut defs: Vec<Definition> = Vec::new();
    let mut refs: Vec<Reference> = Vec::new();
    let mut package_span = Span {
        byte_start: 0,
        byte_end: 0,
        line: 0,
    };

    for (rule_id, node) in tree.matches(rules()) {
        match rule_id {
            "package" => {
                header.package = node
                    .children()
                    .find(|c| c.kind() == "package_identifier")
                    .map(|c| c.text().to_string());
                package_span = span_of(&node);
            }
            "import" => {
                let Some(path_node) = node.field("path") else {
                    continue;
                };
                // Go import paths are string literals: interpreted ("fmt")
                // or raw (`fmt`). Strip whichever quoting was written.
                let path = path_node
                    .text()
                    .trim_matches(|c| c == '"' || c == '`')
                    .to_string();
                let alias = node.field("name").map(|n| n.text().to_string());
                let span = span_of(&node);
                refs.push(Reference {
                    kind: RefKind::Import,
                    space: DeclSpace::Value,
                    raw_target: path.clone(),
                    target: RefTarget {
                        root: TargetRoot::Name,
                        segments: vec![path.clone()],
                    },
                    locally_bound: false,
                    argc: None,
                    enclosing: None,
                    span,
                });
                header.imports.push(Import { alias, path, span });
            }
            "def-func" => {
                let Some(name) = node.field("name") else {
                    continue;
                };
                defs.push(go_def(
                    DefKind::Function,
                    name.text().to_string(),
                    vec![],
                    span_of(&node),
                ));
            }
            "def-method" => {
                let Some(name) = node.field("name") else {
                    continue;
                };
                let owner = node
                    .field("receiver")
                    .and_then(|r| receiver_type_name(&r))
                    .into_iter()
                    .collect();
                defs.push(go_def(
                    DefKind::Method,
                    name.text().to_string(),
                    owner,
                    span_of(&node),
                ));
            }
            "def-type" | "def-const" | "def-var" => {
                if inside_function(&node) {
                    continue; // locals are not nodes
                }
                let kind = match rule_id {
                    "def-type" => DefKind::Type,
                    "def-const" => DefKind::Const,
                    _ => DefKind::Var,
                };
                // const/var specs may declare several names at once;
                // type_spec has exactly one. Collect whichever the
                // grammar provides. `field_children` walks the field's
                // whole run, so it also yields the `,` separators —
                // keep identifiers only, or `const A, B` would define
                // a bogus `,`.
                let mut names: Vec<String> = node
                    .field_children("name")
                    .filter(|n| n.kind() == "identifier")
                    .map(|n| n.text().to_string())
                    .collect();
                if names.is_empty()
                    && let Some(name) = node.field("name")
                {
                    names.push(name.text().to_string());
                }
                for name in names {
                    defs.push(go_def(kind, name, vec![], span_of(&node)));
                }
            }
            "ref-call" => {
                let Some(function) = node.field("function") else {
                    continue;
                };
                // Go's `this`. `t.M()` is the shape every other tier-1
                // language writes `this.M()` and resolves from the declared
                // type, so it carries the same root and gets the same
                // treatment — in both terms of the rate, never on the
                // `local_binding` line. A *bare* `t()` is different: a
                // func-typed receiver called directly names the receiver value
                // itself, which is no more a node than a parameter is.
                let (target, locally_bound) = bind_target(&node, call_target(&function));
                refs.push(Reference {
                    kind: RefKind::Call,
                    space: DeclSpace::Value,
                    raw_target: function.text().to_string(),
                    target,
                    locally_bound,
                    argc: argument_count(&node),
                    enclosing: enclosing_definition(&node),
                    span: span_of(&node),
                });
            }
            "ref-type" => {
                if declares_its_own_name(&node) {
                    continue;
                }
                let (site, target) = type_use_site(&node);
                // Only the *root* of the chain can be bound, exactly as at a
                // call site: `T` is a type parameter or a function-local
                // `type`, and `pkg.T` is rooted at an import.
                let locally_bound = match target.segments.first() {
                    Some(root) => is_locally_bound(&site, root),
                    None => false,
                };
                // `Generic[MyInt](1)` is a call the grammar files as a
                // conversion. Same row, same target, honest kind. A
                // conversion holds exactly one operand, so the arity is not
                // read from the tree — there is nothing else it could be.
                let (kind, argc) = if instantiated_callee(&site) {
                    (RefKind::Call, Some(1))
                } else {
                    (RefKind::TypeUse, None)
                };
                refs.push(Reference {
                    kind,
                    // Go declares types and values in one namespace, so a type
                    // consults the same table a call does.
                    space: DeclSpace::Value,
                    raw_target: site.text().to_string(),
                    target,
                    locally_bound,
                    argc,
                    enclosing: enclosing_definition(&site),
                    span: span_of(&site),
                });
            }
            "ref-select" => {
                if !is_read_site(&node) {
                    continue; // a call's callee, or an inner link of a chain
                }
                let (target, locally_bound) = bind_target(&node, call_target(&node));
                refs.push(Reference {
                    kind: RefKind::FieldAccess,
                    space: DeclSpace::Value,
                    raw_target: node.text().to_string(),
                    target,
                    locally_bound,
                    // Not a call site: `None` and `Some(0)` are different
                    // facts, and a read passes no arguments at all.
                    argc: None,
                    enclosing: enclosing_definition(&node),
                    span: span_of(&node),
                });
            }
            "ref-litkey" => {
                let Some(key) = key_identifier(&node) else {
                    continue; // a map key or an array index: an expression
                };
                let Some(literal_value) = node.parent().filter(|p| p.kind() == "literal_value")
                else {
                    continue;
                };
                let Some(mut segments) = literal_type(&literal_value)
                    .as_ref()
                    .and_then(named_type_path)
                else {
                    continue; // the literal's type names no node
                };
                segments.push(key.text().to_string());
                // The site's own text is `Field`, which two literals of two
                // different types in one function would share — and a
                // `RefKey` that cannot separate them would give both one row
                // and one outcome. So the row carries the target as written,
                // reassembled from the type the literal states and the key:
                // `T.Field`, `pkg.T.Field`.
                let raw_target = segments.join(".");
                // Only the root can be bound, exactly as at a call or a type
                // use: a literal of a function-local `type`, or of a type
                // parameter, names a type that is not a node.
                let locally_bound = is_locally_bound(&key, &segments[0]);
                let span = span_of(&key);
                // Which syntax this row came from, for the resolver: see
                // `GoHeader::literal_keys`.
                header.literal_keys.push(span.byte_start);
                refs.push(Reference {
                    kind: RefKind::FieldAccess,
                    space: DeclSpace::Value,
                    raw_target,
                    target: RefTarget {
                        root: TargetRoot::Name,
                        segments,
                    },
                    locally_bound,
                    argc: None,
                    enclosing: enclosing_definition(&key),
                    span,
                });
            }
            _ => {}
        }
    }

    // The file's package is a definition of its container, emitted whether
    // or not a package clause parsed: a file that lost its clause still
    // belongs to a directory, and the container node is what its references
    // source from. An empty name means "this file does not say", which is
    // not the same as naming the empty string.
    defs.insert(
        0,
        go_def(
            DefKind::Module,
            header.package.clone().unwrap_or_default(),
            vec![],
            package_span,
        ),
    );

    FileFacts { header, defs, refs }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"package server

import (
	"fmt"
	h "net/http"
	_ "embed"
)

const MaxRetries, MinRetries = 5, 1

var registry = newRegistry()

type Handler struct{}

func newRegistry() int { return 0 }

func Serve(addr string) {
	fmt.Println(addr)
	h.ListenAndServe(addr, nil)
	local := func() {}
	local()
}

func (h *Handler) Handle() {
	helper()
	h.reset().apply()
}

func helper() {
	var inner = 1
	_ = inner
}
"#;

    fn facts() -> FileFacts<GoLang> {
        extract("server/server.go", SRC)
    }

    fn call_refs(f: &FileFacts<GoLang>) -> Vec<&Reference> {
        f.refs.iter().filter(|r| r.kind == RefKind::Call).collect()
    }

    #[test]
    fn package_and_imports() {
        let f = facts();
        assert_eq!(f.header.rel_path, "server/server.go");
        assert_eq!(f.header.package.as_deref(), Some("server"));
        let paths: Vec<_> = f.header.imports.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["fmt", "net/http", "embed"]);
        assert_eq!(f.header.imports[1].alias.as_deref(), Some("h"));
        assert_eq!(f.header.imports[2].alias.as_deref(), Some("_"));
    }

    #[test]
    fn imports_are_references_and_header_entries() {
        // The reference is the extractor's; the binding effect is the
        // resolver's. Both halves exist, and the driver resolves only from
        // `refs` — resolving from the header too would count every import
        // twice.
        let f = facts();
        let imports: Vec<_> = f
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Import)
            .collect();
        assert_eq!(imports.len(), 3);
        assert_eq!(f.header.imports.len(), 3);
        for (r, i) in imports.iter().zip(&f.header.imports) {
            assert_eq!(r.raw_target, i.path);
            assert_eq!(
                r.target,
                RefTarget {
                    root: TargetRoot::Name,
                    segments: vec![i.path.clone()],
                }
            );
            assert_eq!(r.enclosing, None);
            assert_eq!(r.span, i.span);
        }
        // `_` and `.` imports are references too: dropping them would lower
        // the denominator the resolution rate is measured against.
        assert!(imports.iter().any(|r| r.raw_target == "embed"));
    }

    #[test]
    fn the_package_clause_is_the_files_container_definition() {
        let f = facts();
        let module = &f.defs[0];
        assert_eq!(module.kind, DefKind::Module);
        assert_eq!(module.name, "server");
        // No package clause: still a container, with no name to offer.
        let g = extract("x/broken.go", "func f() {}\n");
        assert_eq!(g.defs[0].kind, DefKind::Module);
        assert_eq!(g.defs[0].name, "");
    }

    #[test]
    fn raw_string_import_paths_are_unquoted() {
        let f = extract("main.go", "package main\n\nimport `fmt`\n");
        let paths: Vec<_> = f.header.imports.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["fmt"]);
        assert!(
            !f.header
                .imports
                .iter()
                .any(|i| i.path.contains('`') || i.path.contains('"')),
            "import paths keep quote characters: {:?}",
            f.header.imports
        );
    }

    #[test]
    fn multi_name_specs_define_exactly_their_identifiers() {
        let f = extract(
            "main.go",
            "package main\n\nconst A, B = 1, 2\n\nvar X, Y int\n",
        );
        let consts: Vec<_> = f
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::Const)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(consts, ["A", "B"]);
        let vars: Vec<_> = f
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::Var)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(vars, ["X", "Y"]);
    }

    #[test]
    fn package_level_defs_only() {
        let f = facts();
        let names: Vec<_> = f.defs.iter().map(|d| (d.kind, d.name.as_str())).collect();
        assert!(names.contains(&(DefKind::Const, "MaxRetries")));
        assert!(names.contains(&(DefKind::Const, "MinRetries")));
        assert!(names.contains(&(DefKind::Var, "registry")));
        assert!(names.contains(&(DefKind::Type, "Handler")));
        assert!(names.contains(&(DefKind::Function, "Serve")));
        assert!(names.contains(&(DefKind::Function, "helper")));
        assert!(names.contains(&(DefKind::Function, "newRegistry")));
        // locals excluded:
        assert!(!names.iter().any(|(_, n)| *n == "inner" || *n == "local"));
        let method = f.defs.iter().find(|d| d.kind == DefKind::Method).unwrap();
        assert_eq!(method.name, "Handle");
        assert_eq!(method.owner, ["Handler"]);
    }

    #[test]
    fn every_call_site_is_a_reference() {
        let f = facts();
        let calls = call_refs(&f);
        // newRegistry(), fmt.Println, h.ListenAndServe, local(),
        // helper(), h.reset(), h.reset().apply()
        assert_eq!(calls.len(), 7);
        let named = |segments: &[&str]| {
            calls.iter().any(|c| {
                c.target.root == TargetRoot::Name
                    && c.target
                        .segments
                        .iter()
                        .map(String::as_str)
                        .eq(segments.iter().copied())
            })
        };
        assert!(named(&["helper"]));
        assert!(named(&["local"]));
        assert!(named(&["newRegistry"]));
        assert!(named(&["fmt", "Println"]));
        assert!(named(&["h", "ListenAndServe"]));
        // `h.reset()` is the receiver's own method: Go's `this.reset()`, so
        // the root is `This` and the leading `h` is gone.
        assert!(!named(&["h", "reset"]));
        let this: Vec<_> = calls
            .iter()
            .filter(|c| matches!(c.target.root, TargetRoot::This { .. }))
            .collect();
        assert_eq!(this.len(), 1);
        assert_eq!(this[0].raw_target, "h.reset");
        assert_eq!(this[0].target.segments, ["reset"]);
        // h.reset().apply() → the innermost operand is a call, not a name.
        let expr: Vec<_> = calls
            .iter()
            .filter(|c| c.target.root == TargetRoot::Expr)
            .collect();
        assert_eq!(expr.len(), 1);
        assert_eq!(expr[0].raw_target, "h.reset().apply");
        assert_eq!(expr[0].target.segments, ["apply"]);
    }

    #[test]
    fn a_three_segment_chain_is_not_qualified() {
        // `a.b.c()` is three segments under a name root, never two. Landing
        // it in the two-segment arm would send it to the import table and
        // silently reclassify the largest unresolved bucket there is.
        let f = extract(
            "main.go",
            "package main\n\nfunc run() {\n\ta.b.c()\n\tp.q()\n}\n",
        );
        let calls = call_refs(&f);
        let chain = calls.iter().find(|c| c.raw_target == "a.b.c").unwrap();
        assert_eq!(chain.target.root, TargetRoot::Name);
        assert_eq!(chain.target.segments, ["a", "b", "c"]);
        assert_ne!(chain.target.segments.len(), 2);
        let pair = calls.iter().find(|c| c.raw_target == "p.q").unwrap();
        assert_eq!(pair.target.segments, ["p", "q"]);
    }

    #[test]
    fn enclosing_is_a_path_not_a_string() {
        let f = facts();
        let calls = call_refs(&f);
        let helper_call = calls.iter().find(|c| c.raw_target == "helper").unwrap();
        assert_eq!(
            helper_call.enclosing,
            Some(Encloser {
                path: vec!["Handler".into(), "Handle".into()],
                kind: DefKind::Method,
            })
        );
        let inner = calls.iter().find(|c| c.raw_target == "local").unwrap();
        assert_eq!(
            inner.enclosing,
            Some(Encloser {
                path: vec!["Serve".into()],
                kind: DefKind::Function,
            }),
            "a call in a func literal belongs to the named definition around it"
        );
        let registry_init = calls
            .iter()
            .find(|c| c.raw_target == "newRegistry")
            .unwrap();
        assert_eq!(registry_init.enclosing, None); // package-level var init
    }

    #[test]
    fn an_init_body_encloses_at_the_function() {
        // `init` is nameable to the extractor and unnameable to the
        // resolver; the split is deliberate, and the resolver is where it
        // lives.
        let f = extract("boot.go", "package boot\n\nfunc init() {\n\tsetup()\n}\n");
        let calls = call_refs(&f);
        assert_eq!(
            calls[0].enclosing,
            Some(Encloser {
                path: vec!["init".into()],
                kind: DefKind::Function,
            })
        );
    }

    /// Whether the call whose site text is `raw` is a file-local binding.
    fn bound(f: &FileFacts<GoLang>, raw: &str) -> bool {
        call_refs(f)
            .iter()
            .find(|c| c.raw_target == raw)
            .unwrap_or_else(|| panic!("no call site `{raw}`"))
            .locally_bound
    }

    #[test]
    fn a_receiver_shadowing_an_import_is_rooted_at_this_and_not_at_the_name() {
        // The worst of the false-edge bugs, and the pattern is in this
        // module's own fixture: `func (h *Handler)` beside
        // `import h "net/http"`. `h.reset()` names the receiver, so linking
        // it to the import is a wrong edge — strictly worse than an
        // unresolved reference, because a miss is counted and a wrong edge
        // is not.
        //
        // The flag is not what stops it. A receiver is Go's `this`, so the
        // site is re-rooted: there is no leading `h` left for the import
        // table to be asked about, which is a stronger guarantee than
        // "locally bound" ever was, and it is the same shape Java, Python,
        // JavaScript and TypeScript all resolve rather than exclude.
        let f = facts();
        let reset = call_refs(&f)
            .into_iter()
            .find(|c| c.raw_target == "h.reset")
            .expect("no call site `h.reset`");
        assert!(!reset.locally_bound, "a receiver is `this`, not a local");
        assert_eq!(reset.target.root, TargetRoot::This { qualifier: vec![] });
        assert_eq!(reset.target.segments, ["reset"]);
        // The same `h`, one function away, really is the import.
        assert!(!bound(&f, "h.ListenAndServe"));
        assert!(!bound(&f, "fmt.Println"));
    }

    #[test]
    fn an_inner_binder_shadows_the_receiver() {
        // The innermost binder wins, and the receiver is not exempt: once a
        // block re-declares the name, the site names that and nothing else.
        let f = extract(
            "main.go",
            concat!(
                "package main\n\n",
                "type T struct{}\n\n",
                "func (t *T) M() {\n",
                "\tt.before()\n",
                "\tt := other()\n",
                "\tt.after()\n",
                "}\n",
            ),
        );
        let before = call_refs(&f)
            .into_iter()
            .find(|c| c.raw_target == "t.before")
            .expect("`t.before`");
        assert_eq!(before.target.root, TargetRoot::This { qualifier: vec![] });
        assert!(!before.locally_bound);
        assert!(bound(&f, "t.after"), "the short var declaration wins");
    }

    #[test]
    fn a_bare_call_of_a_func_typed_receiver_is_still_a_local_binding() {
        // `type F func(); func (f F) run() { f() }` names the receiver
        // *value*, not a member of it. That value is no more a node than a
        // parameter is, so it stays on the `local_binding` line — the
        // carve-out is for what is selected *through* a receiver.
        let f = extract(
            "main.go",
            concat!(
                "package main\n\n",
                "type F func()\n\n",
                "func (f F) run() {\n\tf()\n}\n",
            ),
        );
        assert!(bound(&f, "f"), "a bare receiver call names the receiver");
    }

    #[test]
    fn a_local_func_value_is_locally_bound() {
        let f = facts();
        assert!(bound(&f, "local"), "a short var declaration binds its name");
        assert!(
            !bound(&f, "helper"),
            "a package-level function is not a local"
        );
    }

    #[test]
    fn a_parameter_is_locally_bound() {
        let f = extract(
            "main.go",
            "package main\n\nfunc Run(helper func()) {\n\thelper()\n}\n\nfunc helper() {}\n",
        );
        assert!(bound(&f, "helper"));
    }

    #[test]
    fn a_binding_after_the_call_does_not_bind_it() {
        // Go starts a declared identifier's scope at the end of its
        // declaration, so the first `x()` names something else entirely.
        let f = extract(
            "main.go",
            "package main\n\nfunc f() {\n\tx()\n\tx := 1\n\t_ = x\n\tx()\n}\n",
        );
        let calls = call_refs(&f);
        let xs: Vec<bool> = calls
            .iter()
            .filter(|c| c.raw_target == "x")
            .map(|c| c.locally_bound)
            .collect();
        assert_eq!(xs, [false, true], "position decides, not presence");
    }

    #[test]
    fn a_sibling_block_binding_does_not_escape() {
        let f = extract(
            "main.go",
            "package main\n\nfunc f(cond bool) {\n\tif cond {\n\t\tx := 1\n\t\t_ = x\n\t}\n\tx()\n}\n",
        );
        assert!(!bound(&f, "x"), "only ancestors bind");
    }

    #[test]
    fn blank_binds_nothing() {
        let f = extract(
            "main.go",
            "package main\n\nfunc f() {\n\t_, err := g()\n\terr()\n\t_()\n}\n",
        );
        assert!(bound(&f, "err"));
        assert!(!bound(&f, "_"), "`_` declares no name");
    }

    #[test]
    fn range_type_switch_and_if_init_bind() {
        let f = extract(
            "main.go",
            concat!(
                "package main\n\n",
                "func f(m map[string]func(), ch chan func(), x any, cond bool) {\n",
                "\tfor k, v := range m {\n\t\tk()\n\t\tv()\n\t}\n",
                "\tif seen := mk(); cond {\n\t\tseen()\n\t} else {\n\t\tseen()\n\t}\n",
                "\tswitch t := x.(type) {\n\tcase int:\n\t\tt()\n\t}\n",
                "\tswitch s := mk(); cond {\n\tcase true:\n\t\ts()\n\t}\n",
                "\tselect {\n\tcase c := <-ch:\n\t\tc()\n\t}\n",
                "\tfor i := 0; i < 3; i++ {\n\t\ti()\n\t}\n",
                "}\n",
            ),
        );
        for name in ["k", "v", "seen", "t", "s", "c", "i"] {
            assert!(bound(&f, name), "`{name}` is bound by its clause");
        }
        assert!(!bound(&f, "mk"), "the initialiser's callee is not bound");
    }

    #[test]
    fn an_assigning_range_clause_binds_nothing() {
        // `for k = range m` assigns to an existing name; only `:=` declares.
        // Reading it as a binder would move a real edge into the local
        // bucket, which raises the rate by deleting a reference from both
        // of its terms.
        let f = extract(
            "main.go",
            "package main\n\nvar k func()\n\nfunc f(m map[int]int) {\n\tfor k = range m {\n\t\tk()\n\t}\n}\n",
        );
        assert!(!bound(&f, "k"));
    }

    #[test]
    fn a_clause_header_does_not_bind_its_own_initialiser() {
        // A declared identifier's scope starts at the end of its
        // declaration, so a clause header's own right-hand side names
        // whatever was in scope before the header — here the package-level
        // function of the same name. Reading the header as binding its own
        // initialiser moves a real reference into the local bucket, which
        // raises the rate by deleting it from both of the rate's terms.
        let f = extract(
            "main.go",
            concat!(
                "package main\n\n",
                "func x() func() { return nil }\n",
                "func v() map[int]func() { return nil }\n",
                "func c() chan func() { return nil }\n",
                "func s() any { return nil }\n\n",
                "func f(cond bool) {\n",
                "\tif x := x(); cond {\n\t\tx()\n\t}\n",
                "\tfor _, v := range v() {\n\t\tv()\n\t}\n",
                "\tselect {\n\tcase c := <-c():\n\t\tc()\n\t}\n",
                "\tswitch s := s().(type) {\n\tcase int:\n\t\t_ = s\n\t}\n",
                "}\n",
            ),
        );
        let sites = |name: &str| -> Vec<bool> {
            call_refs(&f)
                .iter()
                .filter(|r| r.raw_target == name)
                .map(|r| r.locally_bound)
                .collect()
        };
        assert_eq!(sites("x"), [false, true], "if-init RHS, then the body");
        assert_eq!(sites("v"), [false, true], "range RHS, then the body");
        assert_eq!(sites("c"), [false, true], "receive RHS, then the body");
        assert_eq!(sites("s"), [false], "the type-switch guard's own RHS");
    }

    #[test]
    fn an_else_if_initialiser_sees_the_outer_headers_binding() {
        // The two rules meet in one place: an `else if` header does not bind
        // its *own* initialiser, but it sits inside the `if` whose header
        // already closed, so that outer binding does reach it. Walking the
        // ancestors has to reject the inner header and accept the outer one
        // for the same site — getting either half wrong moves a reference
        // between the local bucket and the rate.
        let f = extract(
            "main.go",
            concat!(
                "package main\n\n",
                "func x() func() { return nil }\n\n",
                "func f(cond bool) {\n",
                "\tif x := x(); cond {\n\t\tx()\n",
                "\t} else if y := x(); cond {\n\t\ty()\n\t}\n",
                "}\n",
            ),
        );
        let sites: Vec<bool> = call_refs(&f)
            .iter()
            .filter(|r| r.raw_target == "x")
            .map(|r| r.locally_bound)
            .collect();
        assert_eq!(
            sites,
            [false, true, true],
            "the outer init RHS names the package-level `x`; the consequence \
             body and the else-if's own initialiser both name the one the \
             outer header bound",
        );
        assert!(bound(&f, "y"), "the else-if binds its own body");
    }

    #[test]
    fn named_results_and_receivers_bind_the_whole_body() {
        let f = extract(
            "main.go",
            concat!(
                "package main\n\n",
                "type T struct{}\n\n",
                "func f() (res func(), err error) {\n\tres()\n\treturn\n}\n\n",
                "func (recv *T) M() {\n\trecv()\n}\n",
            ),
        );
        assert!(bound(&f, "res"), "a named result binds its whole body");
        assert!(bound(&f, "recv"), "a receiver binds its whole body");
    }

    #[test]
    fn a_package_level_reference_is_never_locally_bound() {
        let f = facts();
        assert!(!bound(&f, "newRegistry"), "package level binds nothing");
        let g = extract("main.go", "package main\n\nvar registry = registry()\n");
        assert!(!bound(&g, "registry"));
    }

    #[test]
    fn argc_counts_arguments_and_distinguishes_zero_from_unknown() {
        let f = extract(
            "main.go",
            "package main\n\nimport \"fmt\"\n\nfunc f(a, b []int) {\n\tg()\n\tg(1)\n\tg(1, 2)\n\tg(a, b...)\n\t_ = fmt.Sprint\n}\n",
        );
        let argc: Vec<Option<u32>> = call_refs(&f).iter().map(|c| c.argc).collect();
        assert_eq!(argc, [Some(0), Some(1), Some(2), Some(2)]);
        // A spread is one argument: Go does not discriminate by arity, so
        // this is a dedup key component and nothing more.
        let imports: Vec<Option<u32>> = f
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Import)
            .map(|r| r.argc)
            .collect();
        assert_eq!(imports, [None], "an import site has no argument list");
    }
}
