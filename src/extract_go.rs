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
                refs.push(Reference {
                    kind: RefKind::Call,
                    space: DeclSpace::Value,
                    raw_target: function.text().to_string(),
                    target: call_target(&function),
                    locally_bound: false,
                    argc: None,
                    enclosing: enclosing_definition(&node),
                    span: span_of(&node),
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
        assert!(named(&["h", "reset"]));
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

    #[test]
    fn locally_bound_and_argc_are_unset_for_now() {
        // Both fields land with the type layer and are filled by the stage
        // that implements Go's binding environments. Asserting the resting
        // value keeps that stage honest about what it changed.
        let f = facts();
        assert!(f.refs.iter().all(|r| !r.locally_bound));
        assert!(f.refs.iter().all(|r| r.argc.is_none()));
    }
}
