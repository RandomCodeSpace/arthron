//! The single ast-grep touchpoint.
//!
//! ast-grep's Rust API is 0.x and not a stability-guaranteed surface; every
//! import of `ast_grep_*` lives in this file so an upgrade breaks exactly
//! one module. Callers get [`SourceTree`], [`Rules`], and [`SgNode`].

use ast_grep_config::{GlobalRules, RuleConfig, from_yaml_string};
use ast_grep_core::AstGrep;
use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_language::{LanguageExt, SupportLang};

use crate::model::Span;

/// A node in a parsed tree. Alias so callers never name ast-grep types.
pub type SgNode<'r> = ast_grep_core::Node<'r, StrDoc<SupportLang>>;

/// A parsed source file.
pub struct SourceTree {
    inner: AstGrep<StrDoc<SupportLang>>,
}

/// Compiled extraction rules (YAML documents separated by `---`).
pub struct Rules {
    configs: Vec<RuleConfig<SupportLang>>,
}

impl Rules {
    /// Compile a multi-document YAML rule string.
    pub fn compile(yaml: &str) -> Result<Self, String> {
        let configs = from_yaml_string::<SupportLang>(yaml, &GlobalRules::default())
            .map_err(|e| e.to_string())?;
        Ok(Rules { configs })
    }
}

impl SourceTree {
    /// Parse Go source. tree-sitter is error-tolerant: broken files still
    /// yield a tree, with error nodes where parsing failed.
    pub fn parse_go(source: &str) -> Self {
        SourceTree {
            inner: SupportLang::Go.ast_grep(source),
        }
    }

    /// Every `(rule id, node)` pair any rule matches, in rule order.
    pub fn matches<'r>(&'r self, rules: &'r Rules) -> Vec<(&'r str, SgNode<'r>)> {
        let mut out = Vec::new();
        for config in &rules.configs {
            for m in self.inner.root().find_all(&config.matcher) {
                out.push((config.id.as_str(), m.get_node().clone()));
            }
        }
        out
    }
}

/// Convert a node's position into a model [`Span`] (1-based line).
pub fn span_of(node: &SgNode) -> Span {
    let range = node.range();
    Span {
        byte_start: range.start as u32,
        byte_end: range.end as u32,
        line: node.start_pos().line() as u32 + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = r#"
id: fn-decl
language: go
rule:
  kind: function_declaration
"#;

    #[test]
    fn matches_function_declarations_by_kind() {
        let tree = SourceTree::parse_go("package main\n\nfunc main() {}\n");
        let rules = Rules::compile(RULES).expect("rules compile");
        let found = tree.matches(&rules);
        assert_eq!(found.len(), 1);
        let (id, node) = &found[0];
        assert_eq!(*id, "fn-decl");
        assert_eq!(node.kind(), "function_declaration");
        let name = node.field("name").expect("has name field");
        assert_eq!(name.text(), "main");
        assert_eq!(span_of(&name).line, 3);
    }
}
