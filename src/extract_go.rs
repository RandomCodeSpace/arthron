//! Go extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/go.yml`) select nodes by kind; this
//! module interprets their fields. Only package-level declarations become
//! definitions — locals are not nodes.

use std::sync::OnceLock;

use crate::model::{DefKind, Definition, RefKind, RefTarget, Reference, Span};
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

/// Everything extracted from one Go file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileFacts {
    /// The declared package name, if the file parsed far enough to have one.
    pub package: Option<String>,
    /// All import specs.
    pub imports: Vec<Import>,
    /// Package-level definitions only.
    pub defs: Vec<Definition>,
    /// Every call site in the file, wherever it sits.
    pub calls: Vec<Reference>,
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

/// Name of the innermost enclosing definition (`Recv.Name` for methods).
fn enclosing_name(node: &SgNode) -> Option<String> {
    for a in node.ancestors() {
        match &*a.kind() {
            "function_declaration" => {
                return a.field("name").map(|n| n.text().to_string());
            }
            "method_declaration" => {
                let name = a.field("name")?.text().to_string();
                let recv = a
                    .field("receiver")
                    .and_then(|r| receiver_type_name(&r))
                    .unwrap_or_default();
                return Some(format!("{recv}.{name}"));
            }
            _ => {}
        }
    }
    None
}

/// Parse the `function` field of a `call_expression` into a target shape.
fn call_target(function: &SgNode) -> RefTarget {
    match &*function.kind() {
        "identifier" => RefTarget::Plain {
            name: function.text().to_string(),
        },
        "selector_expression" => {
            let operand = function.field("operand");
            let field = function.field("field");
            match (operand, field) {
                (Some(op), Some(f)) if op.kind() == "identifier" => RefTarget::Qualified {
                    qualifier: op.text().to_string(),
                    name: f.text().to_string(),
                },
                _ => RefTarget::Complex,
            }
        }
        _ => RefTarget::Complex,
    }
}

/// Extract all facts from one Go source file.
pub fn extract(source: &str) -> FileFacts {
    let tree = SourceTree::parse_go(source);
    let mut facts = FileFacts::default();

    for (rule_id, node) in tree.matches(rules()) {
        match rule_id {
            "package" => {
                facts.package = node
                    .children()
                    .find(|c| c.kind() == "package_identifier")
                    .map(|c| c.text().to_string());
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
                facts.imports.push(Import {
                    alias,
                    path,
                    span: span_of(&node),
                });
            }
            "def-func" => {
                let Some(name) = node.field("name") else {
                    continue;
                };
                facts.defs.push(Definition {
                    kind: DefKind::Function,
                    name: name.text().to_string(),
                    receiver: None,
                    span: span_of(&node),
                });
            }
            "def-method" => {
                let Some(name) = node.field("name") else {
                    continue;
                };
                let receiver = node.field("receiver").and_then(|r| receiver_type_name(&r));
                facts.defs.push(Definition {
                    kind: DefKind::Method,
                    name: name.text().to_string(),
                    receiver,
                    span: span_of(&node),
                });
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
                    facts.defs.push(Definition {
                        kind,
                        name,
                        receiver: None,
                        span: span_of(&node),
                    });
                }
            }
            "ref-call" => {
                let Some(function) = node.field("function") else {
                    continue;
                };
                facts.calls.push(Reference {
                    kind: RefKind::Call,
                    raw_target: function.text().to_string(),
                    target: call_target(&function),
                    enclosing: enclosing_name(&node),
                    span: span_of(&node),
                });
            }
            _ => {}
        }
    }
    facts
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

    fn facts() -> FileFacts {
        extract(SRC)
    }

    #[test]
    fn package_and_imports() {
        let f = facts();
        assert_eq!(f.package.as_deref(), Some("server"));
        let paths: Vec<_> = f.imports.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["fmt", "net/http", "embed"]);
        assert_eq!(f.imports[1].alias.as_deref(), Some("h"));
        assert_eq!(f.imports[2].alias.as_deref(), Some("_"));
    }

    #[test]
    fn raw_string_import_paths_are_unquoted() {
        let f = extract("package main\n\nimport `fmt`\n");
        let paths: Vec<_> = f.imports.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, ["fmt"]);
        assert!(
            !f.imports
                .iter()
                .any(|i| i.path.contains('`') || i.path.contains('"')),
            "import paths keep quote characters: {:?}",
            f.imports
        );
    }

    #[test]
    fn multi_name_specs_define_exactly_their_identifiers() {
        let f = extract("package main\n\nconst A, B = 1, 2\n\nvar X, Y int\n");
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
        assert_eq!(method.receiver.as_deref(), Some("Handler"));
    }

    #[test]
    fn every_call_site_is_a_reference() {
        let f = facts();
        // newRegistry(), fmt.Println, h.ListenAndServe, local(),
        // helper(), h.reset(), h.reset().apply()
        assert_eq!(f.calls.len(), 7);
        let plain: Vec<_> = f
            .calls
            .iter()
            .filter_map(|c| match &c.target {
                RefTarget::Plain { name } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(plain.contains(&"helper") && plain.contains(&"local"));
        let qualified: Vec<_> = f
            .calls
            .iter()
            .filter_map(|c| match &c.target {
                RefTarget::Qualified { qualifier, name } => {
                    Some((qualifier.as_str(), name.as_str()))
                }
                _ => None,
            })
            .collect();
        assert!(qualified.contains(&("fmt", "Println")));
        assert!(qualified.contains(&("h", "ListenAndServe")));
        assert!(qualified.contains(&("h", "reset")));
        // h.reset().apply() → operand is a call_expression → Complex
        assert!(f.calls.iter().any(|c| c.target == RefTarget::Complex));
    }

    #[test]
    fn enclosing_definition_is_recorded() {
        let f = facts();
        let helper_call = f.calls.iter().find(|c| c.raw_target == "helper").unwrap();
        assert_eq!(helper_call.enclosing.as_deref(), Some("Handler.Handle"));
        let registry_init = f
            .calls
            .iter()
            .find(|c| c.raw_target == "newRegistry")
            .unwrap();
        assert_eq!(registry_init.enclosing, None); // package-level var init
    }
}
