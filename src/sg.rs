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
    /// Parse source in one grammar. tree-sitter is error-tolerant: broken
    /// files still yield a tree, with error nodes where parsing failed, so
    /// every `parse_*` below is total.
    ///
    /// Private because [`SupportLang`] is an ast-grep type and this module is
    /// the only place that may name one. A track picks its grammar by calling
    /// its own `parse_*`.
    fn parse(lang: SupportLang, source: &str) -> Self {
        SourceTree {
            inner: lang.ast_grep(source),
        }
    }

    /// Parse Go source.
    pub fn parse_go(source: &str) -> Self {
        Self::parse(SupportLang::Go, source)
    }

    /// Parse Java source.
    pub fn parse_java(source: &str) -> Self {
        Self::parse(SupportLang::Java, source)
    }

    /// Parse JavaScript source — `.js`, `.mjs` and `.cjs` alike, since the
    /// dialects differ in module semantics rather than in grammar.
    pub fn parse_javascript(source: &str) -> Self {
        Self::parse(SupportLang::JavaScript, source)
    }

    /// Parse TypeScript source, including `.d.ts` declaration files: a
    /// declaration file is a `.ts` file whose bodies happen to be absent, not
    /// a dialect of its own.
    pub fn parse_typescript(source: &str) -> Self {
        Self::parse(SupportLang::TypeScript, source)
    }

    /// Parse Python source.
    pub fn parse_python(source: &str) -> Self {
        Self::parse(SupportLang::Python, source)
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

    /// One rule and one source per grammar the tracks will need.
    ///
    /// The assertion that matters is not the count: it is that the grammar is
    /// compiled into this build at all. `ast-grep-language` ships every
    /// grammar by default, and this is what notices if that ever stops being
    /// true — a missing grammar parses to a tree of error nodes and matches
    /// nothing, which is silent everywhere else.
    fn one_match(tree: &SourceTree, yaml: &str, want_kind: &str, want_name: &str) {
        let rules = Rules::compile(yaml).expect("rules compile");
        let found = tree.matches(&rules);
        assert_eq!(found.len(), 1, "expected one {want_kind}");
        let (_, node) = &found[0];
        assert_eq!(node.kind(), want_kind);
        let name = node.field("name").expect("has name field");
        assert_eq!(name.text(), want_name);
    }

    #[test]
    fn parses_java() {
        one_match(
            &SourceTree::parse_java("class Greeter { void hi() {} }\n"),
            "id: t\nlanguage: java\nrule:\n  kind: class_declaration\n",
            "class_declaration",
            "Greeter",
        );
    }

    #[test]
    fn parses_javascript() {
        let yaml = "id: t\nlanguage: javascript\nrule:\n  kind: function_declaration\n";
        one_match(
            &SourceTree::parse_javascript("export function hi() {}\n"),
            yaml,
            "function_declaration",
            "hi",
        );
        // `.mjs` and `.cjs` are the same grammar: only module semantics differ.
        one_match(
            &SourceTree::parse_javascript("function hi() {}\nmodule.exports = { hi };\n"),
            yaml,
            "function_declaration",
            "hi",
        );
    }

    #[test]
    fn parses_typescript_including_declaration_files() {
        let yaml = "id: t\nlanguage: typescript\nrule:\n  kind: interface_declaration\n";
        one_match(
            &SourceTree::parse_typescript("interface Greeter { hi(): void }\n"),
            yaml,
            "interface_declaration",
            "Greeter",
        );
        // A `.d.ts` file is a `.ts` file; the same parser reads it.
        one_match(
            &SourceTree::parse_typescript("declare interface Greeter { hi(): void }\n"),
            yaml,
            "interface_declaration",
            "Greeter",
        );
    }

    #[test]
    fn parses_python() {
        one_match(
            &SourceTree::parse_python("def hi():\n    pass\n"),
            "id: t\nlanguage: python\nrule:\n  kind: function_definition\n",
            "function_definition",
            "hi",
        );
    }
}
