//! Python extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/python.yml`) select nodes by kind; this
//! module interprets their fields.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_python::lang::PyLang;

/// The embedded Python extraction rules.
const PYTHON_RULES: &str = include_str!("../rules/python.yml");

/// What one import clause binds, and where it reads its name from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportForm {
    /// `import a.b.c [as x]`. Without an alias the statement binds the
    /// *root* (`a`); with one it binds the leaf module (B-01/B-02).
    Module {
        /// The dotted module path, in source order.
        path: Vec<String>,
        /// The `as` name, when written.
        alias: Option<String>,
    },
    /// One name of `from [.]*[module] import name [as alias]`.
    From {
        /// Leading-dot count; `0` for an absolute import (B-05/B-06).
        level: u8,
        /// The module path written after the dots. Empty for `from . import x`.
        module: Vec<String>,
        /// The imported name.
        name: String,
        /// The `as` name, when written.
        alias: Option<String>,
    },
    /// `from [.]*[module] import *` (B-09/B-10).
    Star {
        /// Leading-dot count; `0` for an absolute import.
        level: u8,
        /// The module path written after the dots.
        module: Vec<String>,
    },
}

/// One import clause: what it binds plus where it sits.
///
/// Every `ImportSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is how the
/// resolver pairs the two without the core learning what an import is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    /// What the clause binds.
    pub form: ImportForm,
    /// Where the clause sits.
    pub span: Span,
}

impl ImportSpec {
    /// The name this clause binds in the importing file, or `None` for a
    /// star import, whose bound set is a property of the *imported* module.
    pub fn bound_name(&self) -> Option<&str> {
        match &self.form {
            ImportForm::Module { path, alias } => match alias {
                Some(a) => Some(a.as_str()),
                // `import a.b.c` binds `a`, not `a.b.c` — §7.11, verbatim:
                // "foo, foo.bar, and foo.bar.baz imported, foo bound locally".
                None => path.first().map(String::as_str),
            },
            ImportForm::From { name, alias, .. } => Some(alias.as_deref().unwrap_or(name.as_str())),
            ImportForm::Star { .. } => None,
        }
    }

    /// The dotted path this clause names, without the leading dots.
    pub fn segments(&self) -> Vec<String> {
        match &self.form {
            ImportForm::Module { path, .. } => path.clone(),
            ImportForm::From { module, name, .. } => {
                let mut s = module.clone();
                s.push(name.clone());
                s
            }
            ImportForm::Star { module, .. } => module.clone(),
        }
    }

    /// The literal specifier as written, leading dots included. The store's
    /// dedup key component, so it has to separate `from .a import b` from
    /// `from .b import b` in one file.
    pub fn raw_target(&self) -> String {
        let (level, mut parts) = match &self.form {
            ImportForm::Module { path, .. } => (0, path.clone()),
            ImportForm::From {
                level,
                module,
                name,
                ..
            } => {
                let mut p = module.clone();
                p.push(name.clone());
                (*level, p)
            }
            ImportForm::Star { level, module } => {
                let mut p = module.clone();
                p.push("*".to_string());
                (*level, p)
            }
        };
        let dots = ".".repeat(level as usize);
        if parts.is_empty() {
            parts.push(String::new());
        }
        format!("{dots}{}", parts.join("."))
    }
}

/// Per-file Python facts only the Python resolver reads.
///
/// `rel_path` is here for the same reason Go's is: a Python module's name is
/// a fact about *where the file is*, and the core must not be the layer that
/// turns a path into a module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PyHeader {
    /// Repo-relative, `/`-separated path of the file.
    pub rel_path: String,
    /// The last segment of the module name the path alone decides:
    /// `pkg/__init__.py` → `pkg`, `pkg/sub.py` → `sub` (A-01/A-02).
    pub module_leaf: String,
    /// Whether this file *is* a package — `__init__.py`. The relative-import
    /// anchor differs for one (B-07).
    pub is_package: bool,
    /// Every import clause, in rule-then-source order.
    pub imports: Vec<ImportSpec>,
    /// A literal `__all__` list or tuple of string literals (B-09).
    pub exports: Option<Vec<String>>,
    /// `__all__` is assigned or augmented from something that is not a
    /// literal string sequence, so the export set is not statically
    /// enumerable (B-11).
    pub dynamic_exports: bool,
    /// The module defines `__getattr__`, so it can serve any attribute
    /// (PEP 562, B-14).
    pub has_module_getattr: bool,
    /// The file calls `exec`, `eval` or `globals()`, so names may enter a
    /// namespace with no static declaration site (C-17). Deliberately
    /// conservative: one such call anywhere flags the file, because the
    /// alternative is reporting `NoMatchingDefinition` for a name that does
    /// exist.
    pub has_dynamic_namespace: bool,
    /// The file mutates `sys.path`, so absolute imports elsewhere may mean
    /// something the configured roots cannot express (B-21).
    pub mutates_sys_path: bool,
}

/// The Python extractor. Stateless.
pub struct PyExtractor;

impl Extractor<PyLang> for PyExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<PyLang> {
        extract(rel_path, source)
    }
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| Rules::compile(PYTHON_RULES).expect("embedded python.yml compiles"))
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Whether this file *is* a package rather than a module inside one (B-07).
fn is_package_file(rel_path: &str) -> bool {
    rel_path.rsplit('/').next() == Some("__init__.py")
}

/// The last segment of the module name the path alone decides.
///
/// §5.2.1: a regular package *is* the module object created from its
/// `__init__.py`, so `pkg/__init__.py` is named `pkg` and there is no
/// `pkg.__init__` node (A-01). Which prefix of the directory chain belongs to
/// the name is a project fact the resolver's roots decide, not this one.
fn module_leaf(rel_path: &str) -> String {
    let mut up = rel_path.rsplit('/');
    let file = up.next().unwrap_or("");
    let stem = file.strip_suffix(".py").unwrap_or(file);
    if stem == "__init__" {
        up.next().unwrap_or("").to_string()
    } else {
        stem.to_string()
    }
}

// ---------------------------------------------------------------------------
// Private name mangling (C-13)
// ---------------------------------------------------------------------------

/// Whether §6.2.1's private-name rule applies: "begins with two or more
/// underscore characters and does not end in two or more underscores".
fn is_private_name(name: &str) -> bool {
    name.starts_with("__") && !name.ends_with("__")
}

/// §6.2.1's transformation, applied with the *textually* innermost class.
///
/// `self.__cache` inside `class C` names `C._C__cache`, and a subclass writing
/// the same thing names a different attribute. Declarations are mangled too —
/// `def __m` in `C` stores `_C__m` — so a reference and its declaration agree
/// only if both sides run this.
fn mangle(name: &str, class: Option<&str>) -> String {
    let Some(class) = class else {
        return name.to_string();
    };
    if !is_private_name(name) {
        return name.to_string();
    }
    let stripped = class.trim_start_matches('_');
    if stripped.is_empty() {
        return name.to_string(); // an all-underscore class name mangles nothing
    }
    format!("_{stripped}{name}")
}

/// The name of the textually innermost enclosing class, unmangled.
fn innermost_class_name(node: &SgNode) -> Option<String> {
    node.ancestors()
        .find(|a| a.kind() == "class_definition")
        .and_then(|c| c.field("name"))
        .map(|n| n.text().to_string())
}

// ---------------------------------------------------------------------------
// Lexical position
// ---------------------------------------------------------------------------

/// Kinds that open a block whose bindings are *not* nodes.
///
/// The module block and the class block are absent on purpose: their
/// bindings are module globals and class attributes, both of which a
/// reference elsewhere can name (C-04, C-16).
fn is_binding_block(kind: &str) -> bool {
    matches!(kind, "function_definition" | "lambda") || is_comprehension(kind)
}

/// Kinds that have their own scope and their own `for` targets (C-05).
fn is_comprehension(kind: &str) -> bool {
    matches!(
        kind,
        "list_comprehension"
            | "set_comprehension"
            | "dictionary_comprehension"
            | "generator_expression"
    )
}

/// The class chain enclosing a node, outermost first and mangled, ignoring
/// any functions in between.
fn enclosing_classes(node: &SgNode) -> Vec<String> {
    let mut raw: Vec<String> = node
        .ancestors()
        .filter(|a| a.kind() == "class_definition")
        .filter_map(|a| a.field("name").map(|n| n.text().to_string()))
        .collect();
    raw.reverse();
    let mut out = Vec::with_capacity(raw.len());
    for i in 0..raw.len() {
        let outer = if i == 0 {
            None
        } else {
            Some(raw[i - 1].as_str())
        };
        out.push(mangle(&raw[i], outer));
    }
    out
}

/// The nearest *nameable* enclosing definition.
///
/// Nested `def`s and lambdas are not nodes, so a reference inside one belongs
/// to the named definition around it — the chain is therefore truncated at
/// the outermost function, and everything below it collapses into that
/// function.
fn enclosing_definition(node: &SgNode) -> Option<Encloser> {
    let mut chain: Vec<(String, bool)> = Vec::new();
    for a in node.ancestors() {
        let is_class = match &*a.kind() {
            "class_definition" => true,
            "function_definition" => false,
            _ => continue,
        };
        let name = a.field("name")?.text().to_string();
        chain.push((name, is_class));
    }
    chain.reverse();
    if let Some(first_fn) = chain.iter().position(|(_, is_class)| !is_class) {
        chain.truncate(first_fn + 1);
    }
    let last_is_class = chain.last()?.1;
    let mut path = Vec::with_capacity(chain.len());
    let mut inner: Option<String> = None;
    for (name, is_class) in &chain {
        path.push(mangle(name, inner.as_deref()));
        if *is_class {
            inner = Some(name.clone());
        }
    }
    let kind = if last_is_class {
        DefKind::Type
    } else if chain.len() > 1 {
        DefKind::Method
    } else {
        DefKind::Function
    };
    Some(Encloser { path, kind })
}

/// The first parameter's name, which is what makes `self` a receiver rather
/// than an ordinary variable that happens to be called `self`.
fn first_parameter_name(func: &SgNode) -> Option<String> {
    let params = func.field("parameters")?;
    let p = params.children().find(|c| c.is_named())?;
    match &*p.kind() {
        "identifier" => Some(p.text().to_string()),
        "default_parameter" | "typed_default_parameter" => {
            Some(p.field("name")?.text().to_string())
        }
        "typed_parameter" => p
            .children()
            .find(|c| c.is_named() && c.kind() == "identifier")
            .map(|c| c.text().to_string()),
        _ => None,
    }
}

/// Whether this identifier is the enclosing method's instance or class
/// receiver (E-01/E-02).
///
/// Three conditions, all needed: the name is `self` or `cls`, it is the first
/// parameter of the nearest enclosing function, and that function is a method
/// — a bare function whose first parameter is called `self` binds an ordinary
/// local, and `RefTarget::This` would claim a class that does not exist.
fn is_receiver(node: &SgNode, name: &str) -> bool {
    if name != "self" && name != "cls" {
        return false;
    }
    let Some(func) = node
        .ancestors()
        .find(|a| matches!(&*a.kind(), "function_definition" | "lambda"))
    else {
        return false;
    };
    if func.kind() != "function_definition" {
        return false;
    }
    if first_parameter_name(&func).as_deref() != Some(name) {
        return false;
    }
    func.ancestors()
        .find(|a| {
            matches!(
                &*a.kind(),
                "class_definition" | "function_definition" | "lambda"
            )
        })
        .is_some_and(|a| a.kind() == "class_definition")
}

/// Whether this `call` node is the `super()` of `super().m()` (E-03).
fn is_super_call(call: &SgNode) -> bool {
    call.field("function")
        .is_some_and(|f| f.kind() == "identifier" && f.text() == "super")
}

// ---------------------------------------------------------------------------
// Binding environments (C-01 … C-16)
// ---------------------------------------------------------------------------

/// What one binding block declares.
#[derive(Debug, Default)]
struct BlockScope {
    /// Names bound anywhere in the block. Position is irrelevant: §4.2.1
    /// makes a name bound *anywhere* in a block local *everywhere* in it,
    /// which is the rule Go does not have.
    bound: HashSet<String>,
    /// Names a `global` statement takes back out of the local set (C-07).
    globals: HashSet<String>,
    /// Names a `nonlocal` statement binds in an enclosing function (C-08).
    nonlocals: HashSet<String>,
    /// A comprehension's leading iterable, evaluated in the *enclosing*
    /// scope, so the comprehension's own targets do not bind it (C-05).
    leading_iterable: Option<(usize, usize)>,
}

/// Every binding block in one file, keyed by byte range.
///
/// Precomputed in one pass so that deciding `locally_bound` for a reference
/// is a walk up its ancestors and a hash lookup each, rather than a re-walk
/// of every enclosing body per reference.
#[derive(Debug, Default)]
struct Blocks(HashMap<(usize, usize), BlockScope>);

impl Blocks {
    fn build(root: &SgNode) -> Blocks {
        let mut map = HashMap::new();
        for n in root.dfs() {
            if n.is_named() && is_binding_block(&n.kind()) {
                let r = n.range();
                map.insert((r.start, r.end), block_scope(&n));
            }
        }
        Blocks(map)
    }

    fn get(&self, node: &SgNode) -> Option<&BlockScope> {
        let r = node.range();
        self.0.get(&(r.start, r.end))
    }
}

/// Everything one block binds, without descending into blocks of its own.
fn block_scope(node: &SgNode) -> BlockScope {
    let mut s = BlockScope::default();
    if is_comprehension(&node.kind()) {
        let clauses: Vec<SgNode> = node
            .children()
            .filter(|c| c.kind() == "for_in_clause")
            .collect();
        for c in &clauses {
            if let Some(left) = c.field("left") {
                bind_pattern(&left, &mut s.bound);
            }
        }
        if let Some(right) = clauses.first().and_then(|c| c.field("right")) {
            let r = right.range();
            s.leading_iterable = Some((r.start, r.end));
        }
        return s;
    }
    if let Some(params) = node.field("parameters") {
        bind_parameters(&params, &mut s.bound);
    }
    if let Some(body) = node.field("body") {
        collect_bindings(&body, &mut s);
    }
    s
}

/// Walk a block's own statements, stopping at every nested block.
fn collect_bindings(node: &SgNode, s: &mut BlockScope) {
    for child in node.children().filter(|c| c.is_named()) {
        match &*child.kind() {
            // A nested definition binds *its name* here and owns its body.
            "function_definition" | "class_definition" => {
                if let Some(n) = child.field("name") {
                    s.bound.insert(n.text().to_string());
                }
                continue;
            }
            "decorated_definition" => {
                if let Some(n) = child.field("definition").and_then(|d| d.field("name")) {
                    s.bound.insert(n.text().to_string());
                }
                continue;
            }
            "lambda" => continue,
            k if is_comprehension(k) => {
                // The targets are the comprehension's; a `:=` inside it binds
                // here (C-09).
                collect_walrus(&child, s);
                continue;
            }
            "global_statement" => {
                for n in child.children().filter(|c| c.kind() == "identifier") {
                    s.globals.insert(n.text().to_string());
                }
                continue;
            }
            "nonlocal_statement" => {
                for n in child.children().filter(|c| c.kind() == "identifier") {
                    s.nonlocals.insert(n.text().to_string());
                }
                continue;
            }
            "import_statement" | "import_from_statement" | "future_import_statement" => {
                for spec in import_specs(&child) {
                    if let Some(b) = spec.bound_name() {
                        s.bound.insert(b.to_string());
                    }
                }
                continue;
            }
            "case_clause" => collect_case_captures(&child, &mut s.bound),
            "assignment"
            | "augmented_assignment"
            | "for_statement"
            | "for_in_clause"
            | "type_alias_statement" => {
                if let Some(left) = child.field("left") {
                    bind_pattern(&left, &mut s.bound);
                }
            }
            "as_pattern" => bind_as_alias(&child, &mut s.bound),
            "named_expression" => {
                if let Some(n) = child.field("name") {
                    bind_pattern(&n, &mut s.bound);
                }
            }
            // §4.2.1: a `del` target still counts as bound "for this purpose".
            "delete_statement" => {
                for c in child.children().filter(|c| c.is_named()) {
                    bind_pattern(&c, &mut s.bound);
                }
            }
            _ => {}
        }
        collect_bindings(&child, s);
    }
}

/// Assignment-expression names inside a comprehension, which bind in the
/// containing block rather than the comprehension's own (PEP 572, C-09).
fn collect_walrus(node: &SgNode, s: &mut BlockScope) {
    for child in node.children().filter(|c| c.is_named()) {
        match &*child.kind() {
            "function_definition" | "class_definition" | "decorated_definition" | "lambda" => {
                continue;
            }
            "named_expression" => {
                if let Some(n) = child.field("name") {
                    bind_pattern(&n, &mut s.bound);
                }
            }
            _ => {}
        }
        collect_walrus(&child, s);
    }
}

/// Every name an assignment or `for` target binds. Attribute and subscript
/// targets bind nothing — they mutate an object someone else named.
fn bind_pattern(node: &SgNode, out: &mut HashSet<String>) {
    match &*node.kind() {
        "identifier" => {
            out.insert(node.text().to_string());
        }
        "attribute" | "subscript" => {}
        _ => {
            for c in node.children().filter(|c| c.is_named()) {
                bind_pattern(&c, out);
            }
        }
    }
}

/// The alias of `… as name`. `with` and `except` clauses carry it in an
/// `alias` field; a `case … as name` writes a bare trailing identifier.
fn bind_as_alias(node: &SgNode, out: &mut HashSet<String>) {
    if let Some(alias) = node.field("alias") {
        bind_pattern(&alias, out);
        return;
    }
    if let Some(last) = node.children().filter(|c| c.is_named()).last()
        && last.kind() == "identifier"
    {
        out.insert(last.text().to_string());
    }
}

/// Parameter names, including the splat forms and excluding the `/` and `*`
/// markers, which are named nodes declaring nothing (C-12).
fn bind_parameters(params: &SgNode, out: &mut HashSet<String>) {
    for p in params.children().filter(|c| c.is_named()) {
        match &*p.kind() {
            "positional_separator" | "keyword_separator" => {}
            "default_parameter" | "typed_default_parameter" => {
                if let Some(n) = p.field("name") {
                    bind_pattern(&n, out);
                }
            }
            // The annotation is a sibling of the name, not part of it.
            "typed_parameter" => {
                for c in p.children().filter(|c| c.is_named() && c.kind() != "type") {
                    bind_pattern(&c, out);
                }
            }
            _ => bind_pattern(&p, out),
        }
    }
}

/// Capture names in a `case` clause's patterns.
///
/// A single-identifier `dotted_name` captures; the same node under a
/// `class_pattern` is the class being matched and captures nothing, and a
/// multi-segment `dotted_name` is a value pattern.
fn collect_case_captures(clause: &SgNode, out: &mut HashSet<String>) {
    for pat in clause.children().filter(|c| c.kind() == "case_pattern") {
        capture_names(&pat, out);
    }
}

fn capture_names(node: &SgNode, out: &mut HashSet<String>) {
    for child in node.children().filter(|c| c.is_named()) {
        match &*child.kind() {
            "dotted_name" => {
                let ids: Vec<SgNode> = child
                    .children()
                    .filter(|c| c.kind() == "identifier")
                    .collect();
                if ids.len() == 1 && node.kind() != "class_pattern" {
                    out.insert(ids[0].text().to_string());
                }
            }
            "splat_pattern" => bind_pattern(&child, out),
            "as_pattern" => bind_as_alias(&child, out),
            _ => {}
        }
        capture_names(&child, out);
    }
}

/// Whether some enclosing *function-like* block binds `name` at this site.
///
/// A file-local verdict and the whole of it: the resolver still owns the
/// outcome. Three rules decide it, and none of them is Go's. Position never
/// matters (C-01). The module and class blocks are not consulted, because
/// what they bind is a node (C-04/C-16). And `global` removes a name from
/// the local set for the whole block while `nonlocal` puts it in one
/// (C-07/C-08), so both are checked before the bound set.
fn is_locally_bound(blocks: &Blocks, node: &SgNode, name: &str) -> bool {
    let site = node.range().start;
    for a in node.ancestors() {
        let Some(scope) = blocks.get(&a) else {
            continue;
        };
        if let Some((start, end)) = scope.leading_iterable
            && site >= start
            && site < end
        {
            continue; // evaluated in the enclosing scope
        }
        if scope.globals.contains(name) {
            return false;
        }
        if scope.nonlocals.contains(name) || scope.bound.contains(name) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Imports (B-01 … B-10)
// ---------------------------------------------------------------------------

/// The identifiers of a `dotted_name`, in source order.
fn dotted_parts(node: &SgNode) -> Vec<String> {
    node.children()
        .filter(|c| c.kind() == "identifier")
        .map(|c| c.text().to_string())
        .collect()
}

/// The name clauses of an import statement: the `name` field when the
/// grammar labels one, else every dotted or aliased child after `import`.
fn name_clauses<'r>(stmt: &SgNode<'r>) -> Vec<SgNode<'r>> {
    let labelled: Vec<SgNode> = stmt.field_children("name").collect();
    if !labelled.is_empty() {
        return labelled;
    }
    let after = stmt
        .children()
        .find(|c| c.kind() == "import")
        .map(|c| c.range().end)
        .unwrap_or(0);
    stmt.children()
        .filter(|c| {
            matches!(&*c.kind(), "dotted_name" | "aliased_import") && c.range().start >= after
        })
        .collect()
}

/// The `(level, module)` an `import_from_statement` reads its names from.
fn from_module(stmt: &SgNode) -> (u8, Vec<String>) {
    let Some(m) = stmt.field("module_name") else {
        return (0, Vec::new());
    };
    match &*m.kind() {
        // PEP 328: the leading dots give the level; the anchor is the
        // importing file's `__package__`, which only the resolver knows.
        "relative_import" => {
            let level = m
                .children()
                .find(|c| c.kind() == "import_prefix")
                .map(|p| p.text().chars().filter(|c| *c == '.').count())
                .unwrap_or(0);
            let module = m
                .children()
                .find(|c| c.kind() == "dotted_name")
                .map(|d| dotted_parts(&d))
                .unwrap_or_default();
            (u8::try_from(level).unwrap_or(u8::MAX), module)
        }
        "dotted_name" => (0, dotted_parts(&m)),
        _ => (0, Vec::new()),
    }
}

/// Every clause one import statement binds.
fn import_specs(stmt: &SgNode) -> Vec<ImportSpec> {
    match &*stmt.kind() {
        "import_statement" => name_clauses(stmt).iter().filter_map(module_spec).collect(),
        "import_from_statement" => {
            let (level, module) = from_module(stmt);
            if let Some(star) = stmt.children().find(|c| c.kind() == "wildcard_import") {
                return vec![ImportSpec {
                    form: ImportForm::Star { level, module },
                    span: span_of(&star),
                }];
            }
            name_clauses(stmt)
                .iter()
                .filter_map(|n| from_spec(n, level, &module))
                .collect()
        }
        "future_import_statement" => {
            let module = vec!["__future__".to_string()];
            name_clauses(stmt)
                .iter()
                .filter_map(|n| from_spec(n, 0, &module))
                .collect()
        }
        _ => Vec::new(),
    }
}

fn module_spec(node: &SgNode) -> Option<ImportSpec> {
    let span = span_of(node);
    match &*node.kind() {
        "dotted_name" => Some(ImportSpec {
            form: ImportForm::Module {
                path: dotted_parts(node),
                alias: None,
            },
            span,
        }),
        "aliased_import" => Some(ImportSpec {
            form: ImportForm::Module {
                path: dotted_parts(&node.field("name")?),
                alias: Some(node.field("alias")?.text().to_string()),
            },
            span,
        }),
        _ => None,
    }
}

fn from_spec(node: &SgNode, level: u8, module: &[String]) -> Option<ImportSpec> {
    let span = span_of(node);
    let (name, alias) = match &*node.kind() {
        "dotted_name" => (node.text().to_string(), None),
        "aliased_import" => (
            node.field("name")?.text().to_string(),
            Some(node.field("alias")?.text().to_string()),
        ),
        _ => return None,
    };
    Some(ImportSpec {
        form: ImportForm::From {
            level,
            module: module.to_vec(),
            name,
            alias,
        },
        span,
    })
}

// ---------------------------------------------------------------------------
// Reference targets
// ---------------------------------------------------------------------------

/// Parse a naming expression into a target shape.
///
/// The attribute chain is walked to its innermost operand, exactly as Go
/// walks a selector chain: an identifier there makes the whole dotted path a
/// [`TargetRoot::Name`] target, the method's own receiver makes it
/// [`TargetRoot::This`], a `super()` call makes it [`TargetRoot::Super`], and
/// anything else is [`TargetRoot::Expr`] carrying only the trailing
/// selectors. The *number* of segments survives, so `a.b.c.d()` on a module
/// prefix stays distinguishable from a two-segment qualified name instead of
/// collapsing into one "complex" bucket (E-07).
fn dotted_target(expr: &SgNode) -> RefTarget {
    let mut segments: Vec<String> = Vec::new();
    let mut cur = expr.clone();
    loop {
        match &*cur.kind() {
            "identifier" => {
                let text = cur.text().to_string();
                if is_receiver(&cur, &text) {
                    segments.reverse();
                    return RefTarget {
                        root: TargetRoot::This {
                            qualifier: Vec::new(),
                        },
                        segments,
                    };
                }
                segments.push(text);
                segments.reverse();
                return RefTarget {
                    root: TargetRoot::Name,
                    segments,
                };
            }
            "attribute" => {
                let (Some(object), Some(attribute)) = (cur.field("object"), cur.field("attribute"))
                else {
                    break;
                };
                segments.push(attribute.text().to_string());
                cur = object;
            }
            "call" if is_super_call(&cur) => {
                segments.reverse();
                return RefTarget {
                    root: TargetRoot::Super {
                        qualifier: Vec::new(),
                    },
                    segments,
                };
            }
            "parenthesized_expression" => {
                let Some(inner) = cur.children().find(|c| c.is_named()) else {
                    break;
                };
                cur = inner;
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

/// The number of arguments at a call site.
///
/// Python does not discriminate a callee by arity (G-02: defaults, `*args`,
/// `**kwargs` and keyword-only parameters see to that), so this is a fact
/// about the site rather than a resolution input.
fn argument_count(call: &SgNode) -> Option<u32> {
    let list = call.field("arguments")?;
    let count = list
        .children()
        .filter(|c| c.is_named() && c.kind() != "comment")
        .count();
    u32::try_from(count).ok()
}

/// Build one reference, computing `locally_bound` from the *unmangled* root
/// and mangling the segments afterwards.
///
/// The order is not cosmetic: a block binds the name as written, and §6.2.1
/// mangles the store as well as the load, so both sides agree only when the
/// binding check runs on the source spelling.
fn reference(
    kind: RefKind,
    site: &SgNode,
    raw_target: String,
    mut target: RefTarget,
    argc: Option<u32>,
    span: Span,
    blocks: &Blocks,
) -> Reference {
    let locally_bound = match (&target.root, target.segments.first()) {
        (TargetRoot::Name, Some(root)) => is_locally_bound(blocks, site, root),
        _ => false,
    };
    if kind != RefKind::Import {
        let class = innermost_class_name(site);
        for segment in &mut target.segments {
            *segment = mangle(segment, class.as_deref());
        }
    }
    Reference {
        kind,
        space: DeclSpace::Value,
        raw_target,
        target,
        locally_bound,
        argc,
        enclosing: enclosing_definition(site),
        span,
    }
}

/// Every name an annotation reads, as reference targets.
///
/// PEP 526 and PEP 484 annotations are ordinary expressions naming types, and
/// reading one is *not* type inference — which is why these are `TypeUse`
/// references rather than an entry in the `NeedsTypeInference` bucket (E-05).
/// A string annotation is a forward reference and names the same thing
/// (PEP 563, B-24).
fn annotation_names<'r>(node: &SgNode<'r>, out: &mut Vec<SgNode<'r>>) {
    for child in node.children().filter(|c| c.is_named()) {
        match &*child.kind() {
            // An attribute chain is one name; its parts are not separate
            // references.
            "identifier" | "attribute" | "string" => out.push(child.clone()),
            _ => annotation_names(&child, out),
        }
    }
}

/// The text a string annotation names, when it is a plain dotted name.
fn string_forward_ref(node: &SgNode) -> Option<String> {
    let body: String = node
        .children()
        .filter(|c| c.kind() == "string_content")
        .map(|c| c.text().to_string())
        .collect();
    let ok = !body.is_empty()
        && body.split('.').all(|p| {
            !p.is_empty()
                && p.chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_')
                && p.chars().all(|c| c.is_alphanumeric() || c == '_')
        });
    ok.then_some(body)
}

/// Push a `TypeUse` reference for every name an annotation subtree reads.
fn push_annotation(node: &SgNode, refs: &mut Vec<Reference>, blocks: &Blocks) {
    let mut names = Vec::new();
    annotation_names(node, &mut names);
    for name in names {
        let (raw, target) = if name.kind() == "string" {
            let Some(text) = string_forward_ref(&name) else {
                continue;
            };
            let segments: Vec<String> = text.split('.').map(str::to_string).collect();
            (
                text,
                RefTarget {
                    root: TargetRoot::Name,
                    segments,
                },
            )
        } else {
            (name.text().to_string(), dotted_target(&name))
        };
        refs.push(reference(
            RefKind::TypeUse,
            &name,
            raw,
            target,
            None,
            span_of(&name),
            blocks,
        ));
    }
}

// ---------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------

/// A Python definition. Python declares everything in one space and does not
/// discriminate by arity, so `space` and `params` never vary.
fn py_def(kind: DefKind, name: String, owner: Vec<String>, span: Span) -> Definition {
    let facets = if name.starts_with('_') {
        DefFacets::default()
    } else {
        // §7.11: without `__all__`, a star import takes "all names found in
        // the module's namespace which do not begin with an underscore".
        DefFacets::EXPORTED
    };
    Definition {
        kind,
        name,
        owner,
        space: DeclSpace::Value,
        facets,
        params: None,
        span,
    }
}

/// The head name of each decorator applied to a definition, as written.
fn decorator_names(def: &SgNode) -> Vec<String> {
    let Some(parent) = def.ancestors().next() else {
        return Vec::new();
    };
    if parent.kind() != "decorated_definition" {
        return Vec::new();
    }
    parent
        .children()
        .filter(|c| c.kind() == "decorator")
        .filter_map(|d| decorator_head(&d).map(|h| h.text().to_string()))
        .collect()
}

/// The expression a decorator names, with a decorator-factory call unwrapped
/// to the thing being called (F-01/F-02).
fn decorator_head<'r>(decorator: &SgNode<'r>) -> Option<SgNode<'r>> {
    let expr = decorator.children().find(|c| c.is_named())?;
    if expr.kind() == "call" {
        return expr.field("function");
    }
    Some(expr)
}

/// Whether a decorator name is `x` or `pkg.x`.
fn names_decorator(decorators: &[String], name: &str) -> bool {
    decorators
        .iter()
        .any(|d| d == name || d.rsplit('.').next() == Some(name))
}

/// A `@property`, or one of the accessors that rebind the same name (F-04).
fn is_property_accessor(decorators: &[String]) -> bool {
    names_decorator(decorators, "property")
        || names_decorator(decorators, "cached_property")
        || decorators
            .iter()
            .any(|d| d.ends_with(".setter") || d.ends_with(".getter") || d.ends_with(".deleter"))
}

/// The strings of a literal list or tuple, or `None` when it is anything
/// else — which is the honest answer for a computed `__all__` (B-11).
fn string_literals(node: &SgNode) -> Option<Vec<String>> {
    if !matches!(&*node.kind(), "list" | "tuple") {
        return None;
    }
    let mut out = Vec::new();
    for child in node.children().filter(|c| c.is_named()) {
        if child.kind() != "string" {
            return None;
        }
        out.push(
            child
                .children()
                .filter(|c| c.kind() == "string_content")
                .map(|c| c.text().to_string())
                .collect::<String>(),
        );
    }
    Some(out)
}

/// Names an assignment target binds, in source order and without duplicates.
fn pattern_names(node: &SgNode) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    collect_pattern_names(node, &mut seen, &mut out);
    out
}

fn collect_pattern_names(node: &SgNode, seen: &mut HashSet<String>, out: &mut Vec<String>) {
    match &*node.kind() {
        "identifier" => {
            let name = node.text().to_string();
            if seen.insert(name.clone()) {
                out.push(name);
            }
        }
        "attribute" | "subscript" => {}
        _ => {
            for c in node.children().filter(|c| c.is_named()) {
                collect_pattern_names(&c, seen, out);
            }
        }
    }
}

/// Whether some enclosing function declares `name` global, which makes an
/// assignment to it a *module-level* definition (C-07).
fn declared_global(blocks: &Blocks, node: &SgNode, name: &str) -> bool {
    node.ancestors()
        .filter_map(|a| blocks.get(&a))
        .any(|s| s.globals.contains(name))
}

// ---------------------------------------------------------------------------
// The extractor
// ---------------------------------------------------------------------------

/// Where a statement sits, which decides whether the names it binds are
/// module globals, class attributes, or nothing nameable at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Module,
    ClassBody,
    Function,
}

fn place_of(node: &SgNode) -> Place {
    for a in node.ancestors() {
        match &*a.kind() {
            "function_definition" | "lambda" => return Place::Function,
            "class_definition" => return Place::ClassBody,
            _ => {}
        }
    }
    Place::Module
}

/// Extract all facts from one Python source file.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<PyLang> {
    let tree = SourceTree::parse_python(source);
    let matches = tree.matches(rules());
    let root = matches
        .iter()
        .find(|(id, _)| *id == "module")
        .map(|(_, n)| n.clone());
    let blocks = root.as_ref().map(Blocks::build).unwrap_or_default();

    let mut header = PyHeader {
        rel_path: rel_path.to_string(),
        module_leaf: module_leaf(rel_path),
        is_package: is_package_file(rel_path),
        ..PyHeader::default()
    };
    let mut defs: Vec<Definition> = Vec::new();
    let mut refs: Vec<Reference> = Vec::new();
    let module_span = root.as_ref().map(span_of).unwrap_or(Span {
        byte_start: 0,
        byte_end: 0,
        line: 1,
    });

    for (rule_id, node) in &matches {
        match *rule_id {
            "import" | "import-from" | "import-future" => {
                let at_module = place_of(node) == Place::Module;
                for spec in import_specs(node) {
                    refs.push(reference(
                        RefKind::Import,
                        node,
                        spec.raw_target(),
                        RefTarget {
                            root: TargetRoot::Name,
                            segments: spec.segments(),
                        },
                        None,
                        spec.span,
                        &blocks,
                    ));
                    // A module-level import binding is an attribute of the
                    // module and is nameable from outside it: this is what
                    // makes an `__init__.py` re-export façade work (B-12).
                    if at_module && let Some(bound) = spec.bound_name() {
                        defs.push(py_def(
                            DefKind::Alias,
                            bound.to_string(),
                            Vec::new(),
                            spec.span,
                        ));
                    }
                    header.imports.push(spec);
                }
            }
            "def-function" => {
                let Some(name_node) = node.field("name") else {
                    continue;
                };
                let raw_name = name_node.text().to_string();
                let decorators = decorator_names(node);
                let place = place_of(node);
                if raw_name == "__getattr__" && place == Place::Module {
                    header.has_module_getattr = true; // PEP 562
                }
                if let Some(rt) = node.field("return_type") {
                    push_annotation(&rt, &mut refs, &blocks);
                }
                if let Some(params) = node.field("parameters") {
                    for p in params.children().filter(|c| c.is_named()) {
                        if let Some(t) = p.field("type") {
                            push_annotation(&t, &mut refs, &blocks);
                        }
                    }
                }
                // PEP 484: `@overload` stubs are erased at runtime, so N of
                // them plus one implementation is one definition, not N + 1.
                if names_decorator(&decorators, "overload") {
                    continue;
                }
                if place == Place::Function {
                    continue; // a nested `def` is not nameable from outside
                }
                let owner = enclosing_classes(node);
                let kind = if is_property_accessor(&decorators) {
                    DefKind::Property
                } else if owner.is_empty() {
                    DefKind::Function
                } else {
                    DefKind::Method
                };
                let name = mangle(&raw_name, innermost_class_name(node).as_deref());
                let mut def = py_def(kind, name, owner, span_of(node));
                if names_decorator(&decorators, "staticmethod")
                    || names_decorator(&decorators, "classmethod")
                {
                    def.facets = def.facets.union(DefFacets::STATIC);
                }
                if names_decorator(&decorators, "abstractmethod") {
                    def.facets = def.facets.union(DefFacets::ABSTRACT);
                }
                defs.push(def);
            }
            "def-class" => {
                let Some(name_node) = node.field("name") else {
                    continue;
                };
                // Bases are named whether or not the class itself is a node:
                // the MRO the resolver builds is made of these.
                if let Some(supers) = node.field("superclasses") {
                    for base in supers.children().filter(|c| c.is_named()) {
                        let (kind, expr) = if base.kind() == "keyword_argument" {
                            // `metaclass=M` names M, but M is not a base.
                            match base.field("value") {
                                Some(v) => (RefKind::TypeUse, v),
                                None => continue,
                            }
                        } else {
                            (RefKind::Inherit, base.clone())
                        };
                        refs.push(reference(
                            kind,
                            &expr,
                            expr.text().to_string(),
                            dotted_target(&expr),
                            None,
                            span_of(&expr),
                            &blocks,
                        ));
                    }
                }
                if place_of(node) == Place::Function {
                    continue; // a class inside a function has no canonical name
                }
                let name = mangle(
                    name_node.text().as_ref(),
                    innermost_class_name(node).as_deref(),
                );
                defs.push(py_def(
                    DefKind::Type,
                    name,
                    enclosing_classes(node),
                    span_of(node),
                ));
            }
            "assign" => {
                let Some(left) = node.field("left") else {
                    continue;
                };
                if let Some(t) = node.field("type") {
                    push_annotation(&t, &mut refs, &blocks);
                }
                let place = place_of(node);
                let class = innermost_class_name(node);
                if left.kind() == "attribute" {
                    let object = left.field("object");
                    let attribute = left.field("attribute");
                    let owner = enclosing_classes(node);
                    // D-10: `self.x = v` inside class `C`'s own methods is
                    // the only declaration site `x` has, and without a node
                    // there `self.x()` can never resolve.
                    if let (Some(object), Some(attribute)) = (&object, &attribute)
                        && object.kind() == "identifier"
                        && is_receiver(object, &object.text())
                        && !owner.is_empty()
                    {
                        let name = mangle(&attribute.text(), class.as_deref());
                        defs.push(py_def(DefKind::Field, name, owner, span_of(node)));
                        continue;
                    }
                    // H-03: rebinding someone else's attribute is a naming
                    // site of its own, and it resolves to the same node —
                    // downgrading it would trade a true fact for a caveat.
                    refs.push(reference(
                        RefKind::Rebind,
                        &left,
                        left.text().to_string(),
                        dotted_target(&left),
                        None,
                        span_of(&left),
                        &blocks,
                    ));
                    continue;
                }
                let names = pattern_names(&left);
                if place == Place::Module && names.iter().any(|n| n == "__all__") {
                    match node.field("right").as_ref().and_then(string_literals) {
                        Some(list) => header.exports = Some(list),
                        None => header.dynamic_exports = true,
                    }
                }
                // D-11: `__slots__` entries are attribute declarations with
                // no other syntax to declare them.
                if place == Place::ClassBody
                    && names.iter().any(|n| n == "__slots__")
                    && let Some(list) = node.field("right").as_ref().and_then(string_literals)
                {
                    for slot in list {
                        defs.push(py_def(
                            DefKind::Field,
                            mangle(&slot, class.as_deref()),
                            enclosing_classes(node),
                            span_of(node),
                        ));
                    }
                }
                for name in names {
                    match place {
                        Place::Module => defs.push(py_def(
                            DefKind::Var,
                            mangle(&name, class.as_deref()),
                            Vec::new(),
                            span_of(node),
                        )),
                        Place::ClassBody => defs.push(py_def(
                            DefKind::Field,
                            mangle(&name, class.as_deref()),
                            enclosing_classes(node),
                            span_of(node),
                        )),
                        // C-07: a `global` assignment inside a function
                        // creates a module-level definition. Nothing in Go
                        // does this.
                        Place::Function => {
                            if declared_global(&blocks, node, &name) {
                                defs.push(py_def(DefKind::Var, name, Vec::new(), span_of(node)));
                            }
                        }
                    }
                }
            }
            // `__all__ += submod.__all__` is legal and unenumerable.
            "augassign"
                if place_of(node) == Place::Module
                    && node
                        .field("left")
                        .is_some_and(|l| l.kind() == "identifier" && l.text() == "__all__") =>
            {
                header.dynamic_exports = true;
            }
            "type-alias" => {
                let Some(left) = node.field("left") else {
                    continue;
                };
                if let Some(right) = node.field("right") {
                    push_annotation(&right, &mut refs, &blocks);
                }
                let place = place_of(node);
                if place == Place::Function {
                    continue;
                }
                let class = innermost_class_name(node);
                for name in pattern_names(&left) {
                    let kind = if place == Place::ClassBody {
                        DefKind::Field
                    } else {
                        DefKind::Var
                    };
                    defs.push(py_def(
                        kind,
                        mangle(&name, class.as_deref()),
                        enclosing_classes(node),
                        span_of(node),
                    ));
                }
            }
            "ref-decorator" => {
                let Some(head) = decorator_head(node) else {
                    continue;
                };
                let expr = node.children().find(|c| c.is_named());
                let argc = expr
                    .filter(|e| e.kind() == "call")
                    .and_then(|e| argument_count(&e));
                // §8.7: the decorator expression is evaluated in the block
                // *around* the definition, not inside it — which is why the
                // enclosing scope is read from the decorator node and not
                // from what it decorates.
                refs.push(reference(
                    RefKind::Annotation,
                    &head,
                    head.text().to_string(),
                    dotted_target(&head),
                    argc,
                    span_of(node),
                    &blocks,
                ));
            }
            "ref-call" => {
                let Some(function) = node.field("function") else {
                    continue;
                };
                let target = dotted_target(&function);
                if target.root == TargetRoot::Name {
                    let head = target.segments.first().map(String::as_str);
                    if matches!(head, Some("exec" | "eval" | "globals"))
                        && target.segments.len() == 1
                    {
                        header.has_dynamic_namespace = true; // C-17
                    }
                    if target.segments.len() >= 2
                        && target.segments[target.segments.len() - 2] == "path"
                        && matches!(
                            target.segments[target.segments.len() - 1].as_str(),
                            "append" | "insert" | "extend"
                        )
                        && head == Some("sys")
                    {
                        header.mutates_sys_path = true; // B-21
                    }
                }
                refs.push(reference(
                    RefKind::Call,
                    node,
                    function.text().to_string(),
                    target,
                    argument_count(node),
                    span_of(node),
                    &blocks,
                ));
            }
            _ => {}
        }
    }

    // The module is a definition of the container its own definitions live
    // in, emitted whether or not anything else parsed. Which prefix of the
    // path is part of the name is the resolver's question; that the file
    // declares a module is this layer's answer.
    defs.insert(
        0,
        py_def(
            DefKind::Module,
            header.module_leaf.clone(),
            Vec::new(),
            module_span,
        ),
    );

    FileFacts { header, defs, refs }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(src: &str) -> FileFacts<PyLang> {
        extract("pkg/mod.py", src)
    }

    /// Whether the call site whose literal text is `raw` is a file-local
    /// binding. Panics rather than defaulting: a missing site is a bug in the
    /// fixture, and a silent `false` would assert the opposite of the truth.
    fn bound(f: &FileFacts<PyLang>, raw: &str) -> bool {
        f.refs
            .iter()
            .find(|r| r.kind == RefKind::Call && r.raw_target == raw)
            .unwrap_or_else(|| panic!("no call site `{raw}`"))
            .locally_bound
    }

    /// Every `locally_bound` verdict for the call sites written as `raw`, in
    /// extraction order.
    fn bounds(f: &FileFacts<PyLang>, raw: &str) -> Vec<bool> {
        f.refs
            .iter()
            .filter(|r| r.kind == RefKind::Call && r.raw_target == raw)
            .map(|r| r.locally_bound)
            .collect()
    }

    #[test]
    fn a_name_bound_anywhere_in_a_block_is_local_throughout_it() {
        // §4.2.1, verbatim: "If a name is bound in a block, it is a local
        // variable of that block". Python has no Go-style "scope starts at
        // the end of the declaration" rule, so the call *before* the binding
        // is local too — it is an UnboundLocalError, not a global lookup.
        let f = facts(concat!(
            "def helper(): pass\n",
            "def f():\n",
            "    helper()\n",
            "    helper = lambda: 1\n",
            "    helper()\n",
        ));
        assert_eq!(bounds(&f, "helper"), [true, true]);
    }

    #[test]
    fn a_module_level_name_is_not_a_local() {
        // C-16: the module block's variables are local *and* global, and a
        // global is a node. Calling them locals would delete them from both
        // terms of the resolution rate.
        let f = facts("helper = 1\nhelper()\ndef f():\n    helper()\n");
        assert_eq!(bounds(&f, "helper"), [false, false]);
    }

    #[test]
    fn a_local_shadowing_a_builtin_is_locally_bound() {
        // C-02: builtins are the *last* scope searched, so the block check
        // has to run before any builtin list.
        let f = facts("def f():\n    print = mk()\n    print(1)\n");
        assert!(bound(&f, "print"));
        assert!(!bound(&f, "mk"));
    }

    #[test]
    fn the_class_block_does_not_reach_its_methods() {
        // C-04, verbatim: "The scope of names defined in a class block is
        // limited to the class block; it does not extend to the code blocks
        // of methods."
        let f = facts(concat!(
            "def helper(): pass\n",
            "class C:\n",
            "    helper = None\n",
            "    fallback = helper()\n",
            "    def m(self):\n",
            "        helper()\n",
        ));
        assert_eq!(
            bounds(&f, "helper"),
            [false, false],
            "a class attribute is a node, and it is not in scope in a method",
        );
        // The class-body binding really is a node — the point is that it is
        // a `C.helper` attribute rather than a local, not that it vanished.
        assert!(
            f.defs
                .iter()
                .any(|d| d.kind == DefKind::Field && d.name == "helper" && d.owner == ["C"]),
        );
    }

    #[test]
    fn a_comprehension_binds_its_own_targets() {
        // C-05: a comprehension is a block of its own.
        let f = facts("def f(xs):\n    return [w() for w in xs]\n");
        assert!(bound(&f, "w"));
    }

    #[test]
    fn the_leading_iterable_is_evaluated_in_the_enclosing_scope() {
        // C-05: the leftmost iterable is evaluated outside the comprehension,
        // so the comprehension's own target does not bind it. Reading it as
        // bound would move a real reference into the local bucket.
        let f = facts("def f():\n    return [w for w in w()]\n");
        assert_eq!(bounds(&f, "w"), [false]);
    }

    #[test]
    fn global_makes_a_name_module_level_not_local() {
        // C-07: `global` is the one statement that takes a name *out* of the
        // local set, and it does so for the whole block.
        let f = facts(concat!(
            "def f():\n",
            "    global g\n",
            "    g = mk()\n",
            "    g()\n",
        ));
        assert!(!bound(&f, "g"));
    }

    #[test]
    fn an_inner_function_sees_an_outer_globals_declaration() {
        let f = facts(concat!(
            "def outer():\n",
            "    global g\n",
            "    g = 1\n",
            "    def inner():\n",
            "        g()\n",
        ));
        assert!(!bound(&f, "g"));
    }

    #[test]
    fn nonlocal_names_an_enclosing_functions_local() {
        // C-08: purely intra-function, so never a node.
        let f = facts(concat!(
            "def outer():\n",
            "    v = 1\n",
            "    def inner():\n",
            "        nonlocal v\n",
            "        v()\n",
        ));
        assert!(bound(&f, "v"));
    }

    #[test]
    fn a_closure_variable_is_locally_bound() {
        // C-03: the E of LEGB. Provably in-file, so never a node.
        let f = facts(concat!(
            "def outer():\n",
            "    v = mk()\n",
            "    def inner():\n",
            "        v()\n",
        ));
        assert!(bound(&f, "v"));
    }

    #[test]
    fn a_walrus_in_a_comprehension_binds_the_containing_block() {
        // C-09 / PEP 572: the exception that decides which block gains the
        // name.
        let f = facts(concat!(
            "def f(xs):\n",
            "    [(v := mk(x)) for x in xs]\n",
            "    v()\n",
        ));
        assert!(bound(&f, "v"));
    }

    #[test]
    fn except_with_del_and_for_targets_bind() {
        // C-10, C-11 and §4.2.1's binding-operation list.
        let f = facts(concat!(
            "def f(xs):\n",
            "    for i in xs:\n",
            "        i()\n",
            "    with open(xs) as fh:\n",
            "        fh()\n",
            "    try:\n",
            "        pass\n",
            "    except ValueError as err:\n",
            "        err()\n",
            "    del d\n",
            "    d()\n",
        ));
        for name in ["i", "fh", "err", "d"] {
            assert!(bound(&f, name), "`{name}` is bound by its clause");
        }
    }

    #[test]
    fn parameters_bind_the_whole_body() {
        // C-12, including the positional-only and keyword-only markers,
        // which are named nodes in the grammar and declare nothing.
        let f = facts(concat!(
            "def f(a, /, b=1, *args, c=2, **kw):\n",
            "    a()\n",
            "    b()\n",
            "    args()\n",
            "    c()\n",
            "    kw()\n",
        ));
        for name in ["a", "b", "args", "c", "kw"] {
            assert!(bound(&f, name), "`{name}` is a parameter");
        }
    }

    #[test]
    fn a_lambda_parameter_binds_its_body() {
        let f = facts("def f():\n    return lambda z: z()\n");
        assert!(bound(&f, "z"));
    }

    #[test]
    fn a_function_local_import_binds_a_local() {
        // B-18: `import` inside a function block binds a local, so a
        // same-named module-level use elsewhere must not be caught by it.
        let f = facts(concat!(
            "import os\n",
            "def f():\n",
            "    import json\n",
            "    json.loads()\n",
            "def g():\n",
            "    os.getcwd()\n",
        ));
        assert!(bound(&f, "json.loads"));
        assert!(!bound(&f, "os.getcwd"));
    }

    #[test]
    fn a_sibling_block_binding_does_not_escape() {
        let f = facts(concat!(
            "def f():\n",
            "    def g():\n",
            "        x = 1\n",
            "        return x\n",
            "    x()\n",
        ));
        assert!(!bound(&f, "x"), "only enclosing blocks bind");
    }

    #[test]
    fn a_nested_def_binds_its_name_in_the_enclosing_block() {
        // C-14: the nested function is not a node, so the name that reaches
        // it has to be a local or the call resolves to nothing.
        let f = facts(concat!(
            "def helper(): pass\n",
            "def f():\n",
            "    def helper(): pass\n",
            "    helper()\n",
        ));
        assert!(bound(&f, "helper"));
    }

    #[test]
    fn a_match_capture_binds_and_a_value_pattern_does_not() {
        let f = facts(concat!(
            "def f(q):\n",
            "    match q:\n",
            "        case Point(x=cap):\n",
            "            cap()\n",
            "            Point()\n",
            "        case [a, *rest]:\n",
            "            a()\n",
            "            rest()\n",
            "        case Other() as whole:\n",
            "            whole()\n",
        ));
        for name in ["cap", "a", "rest", "whole"] {
            assert!(bound(&f, name), "`{name}` is a capture pattern");
        }
        assert!(!bound(&f, "Point"), "a class pattern names a class");
    }

    #[test]
    fn self_and_super_roots_are_never_locally_bound() {
        // `self` *is* a parameter, but `self.m` names an attribute of the
        // enclosing class, which is a node. Only a `Name` root consults the
        // binding tables.
        let f = facts(concat!(
            "class C:\n",
            "    def m(self):\n",
            "        self.run()\n",
            "        super().run()\n",
        ));
        assert!(!bound(&f, "self.run"));
        assert!(!bound(&f, "super().run"));
    }

    // -- A. Module identity ------------------------------------------------

    #[test]
    fn the_module_name_the_path_decides() {
        // A-01: a regular package *is* the module made from its
        // `__init__.py`, so there is no `pkg.__init__` node. A-02: a
        // submodule is its stem. A-08: `__main__.py` is an ordinary module.
        let leaf = |p: &str| extract(p, "").header.module_leaf;
        assert_eq!(leaf("pkg/__init__.py"), "pkg");
        assert_eq!(leaf("pkg/a/__init__.py"), "a");
        assert_eq!(leaf("pkg/sub.py"), "sub");
        assert_eq!(leaf("pkg/a/b.py"), "b");
        assert_eq!(leaf("pkg/__main__.py"), "__main__");
        assert_eq!(leaf("main.py"), "main");
        assert!(extract("pkg/__init__.py", "").header.is_package);
        assert!(!extract("pkg/sub.py", "").header.is_package);
    }

    #[test]
    fn the_file_declares_its_module_container() {
        let f = facts("def g(): pass\n");
        assert_eq!(f.defs[0].kind, DefKind::Module);
        assert_eq!(f.defs[0].name, "mod");
        // An unparseable file still belongs to a module: the container node
        // is what its references source from.
        let broken = extract("pkg/mod.py", "def (:\n");
        assert_eq!(broken.defs[0].kind, DefKind::Module);
        assert_eq!(broken.defs[0].name, "mod");
    }

    // -- B. Imports --------------------------------------------------------

    fn imports(f: &FileFacts<PyLang>) -> &[ImportSpec] {
        &f.header.imports
    }

    #[test]
    fn an_unaliased_dotted_import_binds_its_root_and_an_aliased_one_its_leaf() {
        // B-01, verbatim from §7.11: "import foo.bar.baz # foo, foo.bar, and
        // foo.bar.baz imported, foo bound locally". B-02: "foo.bar.baz bound
        // as fbb".
        let f = facts("import a.b.c\nimport a.b.c as x\n");
        let specs = imports(&f);
        assert_eq!(specs.len(), 2);
        assert_eq!(
            specs[0].form,
            ImportForm::Module {
                path: vec!["a".into(), "b".into(), "c".into()],
                alias: None,
            }
        );
        assert_eq!(specs[0].bound_name(), Some("a"));
        assert_eq!(specs[1].bound_name(), Some("x"));
        assert_eq!(specs[0].segments(), ["a", "b", "c"]);
        assert_eq!(specs[0].raw_target(), "a.b.c");
    }

    #[test]
    fn a_from_import_is_one_clause_per_name() {
        // B-03/B-04: each name is its own ordered two-candidate probe —
        // attribute `a.b.c` first, submodule `a.b.c` second — so each is its
        // own reference with its own outcome.
        let f = facts("from a.b import c, d as e\n");
        let specs = imports(&f);
        assert_eq!(specs.len(), 2);
        assert_eq!(
            specs[0].form,
            ImportForm::From {
                level: 0,
                module: vec!["a".into(), "b".into()],
                name: "c".into(),
                alias: None,
            }
        );
        assert_eq!(specs[1].bound_name(), Some("e"));
        assert_eq!(specs[1].segments(), ["a", "b", "d"]);
        assert_eq!(specs[1].raw_target(), "a.b.d");
    }

    #[test]
    fn a_relative_import_keeps_its_level() {
        // B-05/B-06 (PEP 328): the anchor is `__package__`, which only the
        // resolver knows, so the level has to survive extraction. A `String`
        // path could not carry it — `".mod"` is not a resolvable path.
        let f = facts("from . import p\nfrom ..pkg.mod import q as q\nfrom .mod import r\n");
        let specs = imports(&f);
        assert_eq!(
            specs[0].form,
            ImportForm::From {
                level: 1,
                module: vec![],
                name: "p".into(),
                alias: None,
            }
        );
        assert_eq!(specs[0].raw_target(), ".p");
        assert_eq!(
            specs[1].form,
            ImportForm::From {
                level: 2,
                module: vec!["pkg".into(), "mod".into()],
                name: "q".into(),
                alias: Some("q".into()),
            }
        );
        assert_eq!(specs[1].raw_target(), "..pkg.mod.q");
        assert_eq!(specs[2].raw_target(), ".mod.r");
    }

    #[test]
    fn a_star_import_names_its_module_and_binds_nothing_here() {
        // B-09/B-10: what a star import binds is a property of the imported
        // module, not of this file.
        let f = facts("from x.y import *\nfrom . import *\n");
        let specs = imports(&f);
        assert_eq!(
            specs[0].form,
            ImportForm::Star {
                level: 0,
                module: vec!["x".into(), "y".into()],
            }
        );
        assert_eq!(specs[0].bound_name(), None);
        assert_eq!(specs[0].raw_target(), "x.y.*");
        assert_eq!(specs[1].raw_target(), ".*");
    }

    #[test]
    fn a_future_import_is_an_ordinary_import() {
        // B-24: PEP 563 changes what annotations evaluate to, not whether
        // the import names something real.
        let f = facts("from __future__ import annotations\n");
        let specs = imports(&f);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].segments(), ["__future__", "annotations"]);
    }

    #[test]
    fn every_import_clause_is_a_reference_at_the_same_span() {
        // The reference is the extractor's; the binding effect it has is the
        // resolver's. Both halves exist, and the span is what pairs them
        // without the core learning what an import is.
        let f = facts(concat!(
            "import a.b\n",
            "from c import d, e as g\n",
            "from . import h\n",
            "from i import *\n",
        ));
        let import_refs: Vec<&Reference> = f
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Import)
            .collect();
        assert_eq!(import_refs.len(), f.header.imports.len());
        assert_eq!(import_refs.len(), 5);
        for spec in &f.header.imports {
            let paired = import_refs
                .iter()
                .find(|r| r.span == spec.span)
                .expect("every clause has its reference");
            assert_eq!(paired.raw_target, spec.raw_target());
            assert_eq!(paired.target.segments, spec.segments());
            assert_eq!(paired.target.root, TargetRoot::Name);
            assert!(!paired.locally_bound);
            assert_eq!(paired.argc, None);
        }
    }

    #[test]
    fn a_module_level_import_binding_is_a_nameable_alias() {
        // B-12/B-13: `pkg.Foo` and `pkg.core.Foo` name the same definition,
        // and a probe for `pkg.Foo` misses unless the binding is a node in
        // the same keyspace. This is the shape every `__init__.py` façade
        // has.
        let f = extract(
            "pkg/__init__.py",
            "from .core import Foo as Foo\nimport os\ndef g():\n    import json\n",
        );
        let mut aliases: Vec<&str> = f
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::Alias)
            .map(|d| d.name.as_str())
            .collect();
        aliases.sort_unstable();
        assert_eq!(
            aliases,
            ["Foo", "os"],
            "a function-local import binds a local, which is not a node",
        );
    }

    #[test]
    fn a_literal_all_is_a_fact_and_a_computed_one_is_not() {
        // B-09: `__all__` is "a sequence of strings which are names defined
        // or imported by that module". B-11: it need only be that at
        // *runtime*, and no honest reading of a comprehension exists.
        let f = facts("__all__ = [\"a\", \"b\"]\n");
        assert_eq!(
            f.header.exports.as_deref(),
            Some(&["a".to_string(), "b".to_string()][..])
        );
        assert!(!f.header.dynamic_exports);

        let computed = facts("__all__ = [n for n in dir() if not n.startswith(\"_\")]\n");
        assert_eq!(computed.header.exports, None);
        assert!(computed.header.dynamic_exports);

        let augmented = facts("__all__ = [\"a\"]\n__all__ += submod.__all__\n");
        assert!(augmented.header.dynamic_exports);
    }

    #[test]
    fn lazy_dynamic_and_path_mutating_modules_say_so() {
        // B-14 (PEP 562), C-17 and B-21. Each is the difference between
        // "this name does not exist" and "this name cannot be seen from
        // here", and reporting the first when the second is true is the lie
        // the reason taxonomy exists to prevent.
        let lazy = facts("def __getattr__(name):\n    return 1\n");
        assert!(lazy.header.has_module_getattr);
        let method = facts("class C:\n    def __getattr__(self, name):\n        return 1\n");
        assert!(
            !method.header.has_module_getattr,
            "a class `__getattr__` is a fact about the class, not the module",
        );
        assert!(facts("exec(\"x = 1\")\n").header.has_dynamic_namespace);
        assert!(facts("globals()[\"x\"] = 1\n").header.has_dynamic_namespace);
        assert!(!facts("run(\"x\")\n").header.has_dynamic_namespace);
        assert!(facts("sys.path.insert(0, \"x\")\n").header.mutates_sys_path);
        assert!(!facts("os.path.join(a, b)\n").header.mutates_sys_path);
    }

    // -- D. Definitions ----------------------------------------------------

    fn named(f: &FileFacts<PyLang>, kind: DefKind, name: &str) -> Option<Definition> {
        f.defs
            .iter()
            .find(|d| d.kind == kind && d.name == name)
            .cloned()
    }

    const DEFS: &str = concat!(
        "X = 1\n",
        "Y: int = 2\n",
        "Z: int\n",
        "type Alias = int\n",
        "def f(): pass\n",
        "async def g(): pass\n",
        "class C:\n",
        "    attr = 1\n",
        "    RED = \"r\"\n",
        "    __slots__ = (\"s1\", \"s2\")\n",
        "    def m(self, arg):\n",
        "        self.x = arg\n",
        "        local = 1\n",
        "    @staticmethod\n",
        "    def s(): pass\n",
        "    class Inner:\n",
        "        def n(self): pass\n",
        "def outer():\n",
        "    def nested(): pass\n",
        "    class Local: pass\n",
    );

    #[test]
    fn module_and_class_level_names_are_nodes() {
        // D-03, D-04, D-05, D-08, D-09, D-12, D-17.
        let f = facts(DEFS);
        assert!(named(&f, DefKind::Var, "X").is_some());
        assert!(named(&f, DefKind::Var, "Y").is_some());
        assert!(
            named(&f, DefKind::Var, "Z").is_some(),
            "a bare annotation declares"
        );
        assert!(
            named(&f, DefKind::Var, "Alias").is_some(),
            "PEP 695 type alias"
        );
        assert!(named(&f, DefKind::Function, "f").is_some());
        assert!(named(&f, DefKind::Function, "g").is_some(), "async def");
        assert!(named(&f, DefKind::Type, "C").is_some());
        assert_eq!(named(&f, DefKind::Field, "attr").unwrap().owner, ["C"]);
        assert_eq!(named(&f, DefKind::Field, "RED").unwrap().owner, ["C"]);
        assert_eq!(named(&f, DefKind::Method, "m").unwrap().owner, ["C"]);
    }

    #[test]
    fn nesting_puts_the_owner_chain_on_the_definition() {
        // D-06 / C-15: `Outer.Inner` is nameable from anywhere, to arbitrary
        // depth, so one receiver name is not enough.
        let f = facts(DEFS);
        assert_eq!(named(&f, DefKind::Type, "Inner").unwrap().owner, ["C"]);
        assert_eq!(
            named(&f, DefKind::Method, "n").unwrap().owner,
            ["C", "Inner"]
        );
    }

    #[test]
    fn locals_and_things_inside_functions_are_not_nodes() {
        // D-07 / D-16 / C-14: nothing outside can name them, even if they
        // are returned or registered.
        let f = facts(DEFS);
        for name in ["local", "arg", "nested", "Local"] {
            assert!(
                !f.defs.iter().any(|d| d.name == name),
                "`{name}` is not nameable from outside the block",
            );
        }
    }

    #[test]
    fn a_slots_entry_declares_an_attribute() {
        // D-11: the string literals are the only declaration those
        // attributes get.
        let f = facts(DEFS);
        for slot in ["s1", "s2"] {
            assert_eq!(named(&f, DefKind::Field, slot).unwrap().owner, ["C"]);
        }
    }

    #[test]
    fn a_self_assignment_declares_an_attribute_on_the_enclosing_class() {
        // D-10, the contested one: `self.x = v` is not a declaration, but
        // `obj.x` is a naming site and nothing else declares `x`. Without a
        // node there, `self.x()` can never resolve.
        let f = facts(DEFS);
        let x = named(&f, DefKind::Field, "x").expect("self.x declares C.x");
        assert_eq!(x.owner, ["C"]);
        // And it is a declaration, not a rebinding of someone else's
        // attribute.
        assert!(
            !f.refs
                .iter()
                .any(|r| r.kind == RefKind::Rebind && r.raw_target == "self.x"),
        );
    }

    #[test]
    fn a_global_assignment_inside_a_function_declares_a_module_level_name() {
        // C-07: definition sites are no longer confined to module and class
        // bodies. No Go analogue.
        let f = facts("def f():\n    global registry\n    registry = {}\n");
        let def = named(&f, DefKind::Var, "registry").expect("a module-level definition");
        assert!(def.owner.is_empty());
        // Without the `global`, the same assignment declares nothing.
        let local = facts("def f():\n    registry = {}\n");
        assert!(named(&local, DefKind::Var, "registry").is_none());
    }

    #[test]
    fn a_tuple_assignment_declares_each_name() {
        let f = facts("A, (B, C) = mk()\n[D] = mk()\n");
        for name in ["A", "B", "C", "D"] {
            assert!(named(&f, DefKind::Var, name).is_some(), "`{name}`");
        }
    }

    #[test]
    fn the_public_name_convention_is_a_facet() {
        // §7.11: without `__all__`, a star import takes "all names found in
        // the module's namespace which do not begin with an underscore".
        let f = facts("def public(): pass\ndef _private(): pass\n");
        assert!(
            named(&f, DefKind::Function, "public")
                .unwrap()
                .facets
                .contains(DefFacets::EXPORTED),
        );
        assert!(
            !named(&f, DefKind::Function, "_private")
                .unwrap()
                .facets
                .contains(DefFacets::EXPORTED),
        );
    }

    // -- C-13. Private name mangling ---------------------------------------

    #[test]
    fn private_names_are_mangled_by_the_innermost_class() {
        // §6.2.1: `self.__cache` inside `class C` names `C._C__cache`, and a
        // subclass writing the same thing names a *different* attribute.
        // Both the store and the load are mangled, or the two never meet.
        let f = facts(concat!(
            "class C:\n",
            "    def __init__(self):\n",
            "        self.__cache = {}\n",
            "    def __helper(self): pass\n",
            "    def m(self):\n",
            "        self.__helper()\n",
            "        self.__dunder__()\n",
        ));
        assert_eq!(named(&f, DefKind::Field, "_C__cache").unwrap().owner, ["C"]);
        assert!(named(&f, DefKind::Method, "_C__helper").is_some());
        let call = f
            .refs
            .iter()
            .find(|r| r.kind == RefKind::Call && r.raw_target == "self.__helper")
            .unwrap();
        assert_eq!(call.target.segments, ["_C__helper"]);
        // A name ending in two underscores is not private.
        let dunder = f
            .refs
            .iter()
            .find(|r| r.raw_target == "self.__dunder__")
            .unwrap();
        assert_eq!(dunder.target.segments, ["__dunder__"]);
    }

    #[test]
    fn a_subclass_writing_the_same_private_name_writes_a_different_one() {
        let f = facts(concat!(
            "class Base:\n",
            "    def m(self):\n",
            "        self.__x()\n",
            "class Sub(Base):\n",
            "    def m(self):\n",
            "        self.__x()\n",
        ));
        let mangled: Vec<&str> = f
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Call && r.raw_target == "self.__x")
            .map(|r| r.target.segments[0].as_str())
            .collect();
        assert_eq!(mangled, ["_Base__x", "_Sub__x"]);
    }

    // -- E. Attribute access -----------------------------------------------

    fn call(f: &FileFacts<PyLang>, raw: &str) -> Reference {
        f.refs
            .iter()
            .find(|r| r.kind == RefKind::Call && r.raw_target == raw)
            .unwrap_or_else(|| panic!("no call site `{raw}`"))
            .clone()
    }

    #[test]
    fn a_receiver_call_is_a_this_root_and_super_is_its_own() {
        // E-01/E-02/E-03: the lexically enclosing class is statically known,
        // which is what makes this Python's largest resolvable call class —
        // and `RefTarget::Name` with a qualifier `self` would be
        // indistinguishable from a variable called `self`.
        let f = facts(concat!(
            "class C:\n",
            "    def m(self):\n",
            "        self.run()\n",
            "        super().setup()\n",
            "    @classmethod\n",
            "    def k(cls):\n",
            "        cls.build()\n",
        ));
        assert_eq!(
            call(&f, "self.run").target,
            RefTarget {
                root: TargetRoot::This { qualifier: vec![] },
                segments: vec!["run".into()],
            }
        );
        assert_eq!(
            call(&f, "cls.build").target,
            RefTarget {
                root: TargetRoot::This { qualifier: vec![] },
                segments: vec!["build".into()],
            }
        );
        assert_eq!(
            call(&f, "super().setup").target,
            RefTarget {
                root: TargetRoot::Super { qualifier: vec![] },
                segments: vec!["setup".into()],
            }
        );
    }

    #[test]
    fn a_variable_called_self_is_not_a_receiver() {
        // The three conditions are all load-bearing: `self` must be the
        // first parameter of the nearest enclosing function, and that
        // function must be a method. A `This` root here would claim a class
        // that does not exist.
        let f = facts(concat!(
            "def free(self):\n",
            "    self.run()\n",
            "class C:\n",
            "    def m(self, other):\n",
            "        other.run()\n",
        ));
        assert_eq!(call(&f, "self.run").target.root, TargetRoot::Name);
        assert_eq!(call(&f, "self.run").target.segments, ["self", "run"]);
        assert!(call(&f, "self.run").locally_bound, "a parameter is a local");
        assert_eq!(call(&f, "other.run").target.root, TargetRoot::Name);
    }

    #[test]
    fn an_attribute_chain_keeps_every_segment() {
        // E-07: `a.b.c.d()` on a module prefix is statically resolvable, and
        // collapsing it into one "complex" bucket reports it under the one
        // label that means "unresolvable without machinery we do not have".
        let f = facts("a.b.c.d()\nmod.f()\nC.m()\nf().m()\nd[\"k\"].m()\n");
        let chain = call(&f, "a.b.c.d");
        assert_eq!(chain.target.root, TargetRoot::Name);
        assert_eq!(chain.target.segments, ["a", "b", "c", "d"]);
        assert_eq!(call(&f, "mod.f").target.segments, ["mod", "f"]);
        assert_eq!(call(&f, "C.m").target.segments, ["C", "m"]);
        // A member on an expression result really does need a type.
        let on_call = call(&f, "f().m");
        assert_eq!(on_call.target.root, TargetRoot::Expr);
        assert_eq!(on_call.target.segments, ["m"]);
        assert_eq!(call(&f, "d[\"k\"].m").target.root, TargetRoot::Expr);
    }

    #[test]
    fn argc_counts_arguments_and_distinguishes_zero_from_unknown() {
        // G-02: Python has no compile-time overloading, so this is a fact
        // about the site rather than a resolution input.
        let f = facts("g()\ng(1)\ng(1, 2)\ng(*a, **k)\n");
        let argc: Vec<Option<u32>> = f
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Call)
            .map(|r| r.argc)
            .collect();
        assert_eq!(argc, [Some(0), Some(1), Some(2), Some(2)]);
        assert!(
            f.defs.iter().all(|d| d.params.is_none()),
            "arity is not part of a Python definition's identity",
        );
    }

    // -- F. Decorators -----------------------------------------------------

    #[test]
    fn a_decorator_is_a_reference_evaluated_outside_what_it_decorates() {
        // F-01: a bare `@d` is not a call expression and a call-shaped rule
        // would not see it at all; `@d(x)` would be seen but attributed to
        // the wrong scope, because §8.7 evaluates the expression in the
        // block around the definition.
        let f = facts(concat!(
            "@deco\n",
            "@app.route(\"/\", methods=[\"GET\"])\n",
            "def handler(): pass\n",
            "class C:\n",
            "    @property\n",
            "    def x(self): return 1\n",
        ));
        let decorators: Vec<&Reference> = f
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Annotation)
            .collect();
        let bare = decorators.iter().find(|r| r.raw_target == "deco").unwrap();
        assert_eq!(bare.target.segments, ["deco"]);
        assert_eq!(bare.argc, None);
        assert_eq!(bare.enclosing, None, "evaluated at module level");
        let factory = decorators
            .iter()
            .find(|r| r.raw_target == "app.route")
            .unwrap();
        assert_eq!(factory.target.segments, ["app", "route"]);
        assert_eq!(factory.argc, Some(2));
        let on_method = decorators
            .iter()
            .find(|r| r.raw_target == "property")
            .unwrap();
        assert_eq!(
            on_method.enclosing,
            Some(Encloser {
                path: vec!["C".into()],
                kind: DefKind::Type,
            }),
            "a method's decorator runs in the class block, not in the method",
        );
    }

    #[test]
    fn staticmethod_classmethod_and_abstractmethod_are_facets() {
        // F-03: still nodes at `C.m`; what changes is the binding of the
        // first parameter, which E-02 reads.
        let f = facts(concat!(
            "class C:\n",
            "    @staticmethod\n",
            "    def s(): pass\n",
            "    @classmethod\n",
            "    def k(cls): pass\n",
            "    @abc.abstractmethod\n",
            "    def a(self): pass\n",
        ));
        assert!(
            named(&f, DefKind::Method, "s")
                .unwrap()
                .facets
                .contains(DefFacets::STATIC),
        );
        assert!(
            named(&f, DefKind::Method, "k")
                .unwrap()
                .facets
                .contains(DefFacets::STATIC),
        );
        assert!(
            named(&f, DefKind::Method, "a")
                .unwrap()
                .facets
                .contains(DefFacets::ABSTRACT),
            "a dotted decorator name resolves by its last segment",
        );
    }

    #[test]
    fn a_property_and_its_setter_are_one_name_declared_twice() {
        // F-04: `@x.setter` rebinds the same name, so two `def x` in one
        // class body are one attribute with two sites.
        let f = facts(concat!(
            "class C:\n",
            "    @property\n",
            "    def x(self): return self._x\n",
            "    @x.setter\n",
            "    def x(self, v): self._x = v\n",
        ));
        let sites: Vec<&Definition> = f
            .defs
            .iter()
            .filter(|d| d.name == "x" && d.kind == DefKind::Property)
            .collect();
        assert_eq!(sites.len(), 2, "one name, two declaration sites");
        assert_eq!(sites[0].owner, ["C"]);
    }

    #[test]
    fn an_overload_stub_is_not_a_declaration_site() {
        // F-06 (PEP 484): the stubs are erased at runtime — the last `def`
        // wins. N definition upserts under one FQN would leave the graph
        // pointing at a stub.
        let f = facts(concat!(
            "@typing.overload\n",
            "def read(p: str) -> str: ...\n",
            "@overload\n",
            "def read(p: int) -> bytes: ...\n",
            "def read(p): return p\n",
        ));
        let sites: Vec<&Definition> = f.defs.iter().filter(|d| d.name == "read").collect();
        assert_eq!(sites.len(), 1, "one definition, not three");
        // The `@overload` decorators themselves are still real references.
        assert_eq!(
            f.refs
                .iter()
                .filter(|r| r.kind == RefKind::Annotation)
                .count(),
            2,
        );
    }

    // -- Inheritance, rebinding, annotations --------------------------------

    #[test]
    fn base_classes_are_inherit_references_and_a_metaclass_is_not() {
        // The MRO the resolver builds is made of these; a metaclass names
        // something real but is not a base.
        let f = facts("class C(Base, mixins.Loud, metaclass=Meta):\n    pass\n");
        let bases: Vec<Vec<String>> = f
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Inherit)
            .map(|r| r.target.segments.clone())
            .collect();
        assert_eq!(bases, [vec!["Base"], vec!["mixins", "Loud"]]);
        let meta = f
            .refs
            .iter()
            .find(|r| r.kind == RefKind::TypeUse && r.raw_target == "Meta")
            .unwrap();
        assert_eq!(meta.target.segments, ["Meta"]);
    }

    #[test]
    fn rebinding_someone_elses_attribute_is_its_own_reference() {
        // H-03: do *not* downgrade the call edges to `f`; record the
        // rebinding, so `impact` can report that `f` is reassigned. It
        // resolves to the same node.
        let f = facts("mod.f = replacement\nC.method = f\nd[\"k\"] = v\n");
        let rebinds: Vec<(&str, Vec<String>)> = f
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Rebind)
            .map(|r| (r.raw_target.as_str(), r.target.segments.clone()))
            .collect();
        assert_eq!(
            rebinds,
            [
                ("mod.f", vec!["mod".to_string(), "f".to_string()]),
                ("C.method", vec!["C".to_string(), "method".to_string()]),
            ],
            "a subscript target names no definition",
        );
    }

    #[test]
    fn an_annotation_is_a_type_use_and_not_type_inference() {
        // E-05: `def f(c: Client): c.send()` is resolvable with no inference
        // at all — read the annotation and resolve the name. Routing these
        // to `NeedsTypeInference` would hide a fixable problem behind an
        // unfixable-sounding label.
        let f = facts(concat!(
            "def f(p: Client, q: list[Item] = None) -> pkg.Ret:\n",
            "    pass\n",
            "x: \"Forward\" = None\n",
            "y: int\n",
        ));
        let uses: Vec<Vec<String>> = f
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::TypeUse)
            .map(|r| r.target.segments.clone())
            .collect();
        for want in [
            vec!["Client".to_string()],
            vec!["list".to_string()],
            vec!["Item".to_string()],
            vec!["pkg".to_string(), "Ret".to_string()],
            vec!["Forward".to_string()],
            vec!["int".to_string()],
        ] {
            assert!(uses.contains(&want), "annotation `{want:?}` is a reference");
        }
    }

    // -- The reference's own scope ------------------------------------------

    #[test]
    fn enclosing_is_a_path_and_a_nested_block_collapses_into_it() {
        // §1.5 / J6: `C.m` is not expressible as one unqualified string, and
        // a nested `def` is not a node, so a call inside one belongs to the
        // named definition around it.
        let f = facts(concat!(
            "top()\n",
            "class C:\n",
            "    attr = body()\n",
            "    def m(self):\n",
            "        inner()\n",
            "        def helper():\n",
            "            deep()\n",
            "        [x() for x in xs]\n",
            "    class Inner:\n",
            "        def n(self):\n",
            "            deepest()\n",
            "def free():\n",
            "    plain()\n",
        ));
        assert_eq!(call(&f, "top").enclosing, None);
        assert_eq!(
            call(&f, "body").enclosing,
            Some(Encloser {
                path: vec!["C".into()],
                kind: DefKind::Type,
            }),
        );
        let method = Some(Encloser {
            path: vec!["C".into(), "m".into()],
            kind: DefKind::Method,
        });
        assert_eq!(call(&f, "inner").enclosing, method);
        assert_eq!(
            call(&f, "deep").enclosing,
            method,
            "a nested def is not a node"
        );
        assert_eq!(call(&f, "x").enclosing, method, "nor is a comprehension");
        assert_eq!(
            call(&f, "deepest").enclosing,
            Some(Encloser {
                path: vec!["C".into(), "Inner".into(), "n".into()],
                kind: DefKind::Method,
            }),
        );
        assert_eq!(
            call(&f, "plain").enclosing,
            Some(Encloser {
                path: vec!["free".into()],
                kind: DefKind::Function,
            }),
        );
    }

    #[test]
    fn every_python_record_declares_in_one_space() {
        // Python has one namespace; the two axes still exist, and this is
        // the answer on the second one.
        let f = facts(DEFS);
        assert!(f.defs.iter().all(|d| d.space == DeclSpace::Value));
        assert!(f.refs.iter().all(|r| r.space == DeclSpace::Value));
    }
}
