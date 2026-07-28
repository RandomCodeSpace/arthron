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

    /// Parse Ruby source.
    ///
    /// One grammar for every `.rb` file: a gemspec, a Rakefile and a library
    /// file are the same language, and only the walk decides which of them a
    /// scan reads.
    pub fn parse_ruby(source: &str) -> Self {
        Self::parse(SupportLang::Ruby, source)
    }

    /// Parse PHP source.
    ///
    /// One grammar for the whole file, including the text outside `<?php`
    /// tags: tree-sitter-php's `php` dialect reads a template file the way
    /// the interpreter does, so an extractor never has to find the tags
    /// itself.
    pub fn parse_php(source: &str) -> Self {
        Self::parse(SupportLang::Php, source)
    }

    /// Parse C# source, exactly as written.
    ///
    /// No preprocessing: `#if` is a directive in the tree rather than a
    /// filter over it, so **both arms of a conditional are parsed** and both
    /// contribute declarations. Choosing an arm would mean choosing a target
    /// framework, and a scan that read one build of a multi-targeted project
    /// would report a graph that no single reader of the source could see.
    pub fn parse_csharp(source: &str) -> Self {
        Self::parse(SupportLang::CSharp, source)
    }

    /// Parse Rust source.
    pub fn parse_rust(source: &str) -> Self {
        Self::parse(SupportLang::Rust, source)
    }

    /// Parse Kotlin source — `.kt` and `.kts` alike.
    ///
    /// One grammar for both: a Gradle build script is Kotlin whose top level
    /// happens to be statements rather than declarations, not a dialect of
    /// its own, and the walk is what decides which files a scan reads.
    pub fn parse_kotlin(source: &str) -> Self {
        Self::parse(SupportLang::Kotlin, source)
    }

    /// Parse Bash source — `.sh` and `.bash` alike.
    ///
    /// One grammar for both: the extension records what a repository calls a
    /// script and what it calls a sourced library, not which dialect it is
    /// written in. `.bats` is **not** parsed here and is not claimed by the
    /// language: the shell grammar does not reject a `@test "name" { … }`
    /// block, it misreads one — see [`crate::model::Lang::extensions`].
    pub fn parse_bash(source: &str) -> Self {
        Self::parse(SupportLang::Bash, source)
    }

    /// Parse Scala source.
    ///
    /// One grammar for every dialect: Scala 2 and Scala 3 differ in surface
    /// syntax the same tree-sitter grammar reads — `import a._` and `import
    /// a.*` are both wildcards, `=>` and `as` are both renames — and a
    /// repository that cross-builds writes both in one tree.
    pub fn parse_scala(source: &str) -> Self {
        Self::parse(SupportLang::Scala, source)
    }

    /// Parse Swift source.
    ///
    /// One grammar for every `.swift` file, a SwiftPM manifest included: a
    /// `Package.swift` is an ordinary Swift program that the package manager
    /// runs, not a configuration dialect, and reading it under a second
    /// parser would be inventing a language the toolchain does not have.
    pub fn parse_swift(source: &str) -> Self {
        Self::parse(SupportLang::Swift, source)
    }

    /// Parse C++ source.
    ///
    /// One grammar for every extension [`crate::model::Lang::Cpp`] claims.
    /// `.c` and `.h` are deliberately not among them — a C translation unit
    /// read under the C++ grammar is the wrong language — so nothing here
    /// has to guess which dialect a file is written in.
    pub fn parse_cpp(source: &str) -> Self {
        Self::parse(SupportLang::Cpp, source)
    }

    /// Parse HCL source.
    ///
    /// One grammar for every HCL dialect the extension list claims — today
    /// only `.tf`. The grammar reads a Terraform configuration as a flat
    /// sequence of `block`s and `attribute`s and knows nothing about which
    /// block types Terraform gives meaning to, which is why every such
    /// judgement is the extractor's and none of it is here.
    pub fn parse_hcl(source: &str) -> Self {
        Self::parse(SupportLang::Hcl, source)
    }

    /// Parse Lua source.
    ///
    /// One grammar for every dialect: LuaJIT and PUC Lua 5.1 through 5.4
    /// differ in library surface and in `goto`/integer-division spellings the
    /// same tree-sitter grammar reads, and a repository that supports several
    /// runtimes writes them in one tree. A rockspec is Lua too — it is a
    /// chunk of assignments — and phase 0 parses it with this same function
    /// rather than pattern-matching its bytes.
    pub fn parse_lua(source: &str) -> Self {
        Self::parse(SupportLang::Lua, source)
    }

    /// Parse a YAML document.
    ///
    /// Not a source language and not claimed by any track: `pubspec.yaml` is
    /// Dart's manifest, and phase 0 reads it with the grammar its own format
    /// has rather than with a line scanner. No walk ever reaches a `.yaml`
    /// file — [`crate::model::Lang::for_extension`] answers `None` for it —
    /// so this parses a manifest a resolver names explicitly and nothing
    /// else.
    pub fn parse_yaml(source: &str) -> Self {
        Self::parse(SupportLang::Yaml, source)
    }

    /// Parse Dart source.
    ///
    /// One grammar for every `.dart` file: Dart has no dialects, and a
    /// library, a `part` file and a test are the same language read the same
    /// way — only the walk decides which of them a scan reads.
    pub fn parse_dart(source: &str) -> Self {
        Self::parse(SupportLang::Dart, source)
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
    fn parses_php() {
        one_match(
            &SourceTree::parse_php("<?php\nclass Greeter { public function hi() {} }\n"),
            "id: t\nlanguage: php\nrule:\n  kind: class_declaration\n",
            "class_declaration",
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

    #[test]
    fn parses_ruby() {
        one_match(
            &SourceTree::parse_ruby("module Rack\n  class Request\n  end\nend\n"),
            "id: t\nlanguage: ruby\nrule:\n  kind: module\n",
            "module",
            "Rack",
        );
    }

    #[test]
    fn parses_csharp() {
        one_match(
            &SourceTree::parse_csharp("namespace N;\nclass Greeter { void Hi() {} }\n"),
            "id: t\nlanguage: csharp\nrule:\n  kind: class_declaration\n",
            "class_declaration",
            "Greeter",
        );
    }

    #[test]
    fn parses_rust() {
        one_match(
            &SourceTree::parse_rust("fn hi() {}\n"),
            "id: t\nlanguage: rust\nrule:\n  kind: function_item\n",
            "function_item",
            "hi",
        );
    }

    /// Kotlin, which [`one_match`] cannot check: tree-sitter-kotlin names no
    /// `name` field on a declaration, so the name is a child of a known kind
    /// rather than a field. The point of the test is the same — the grammar
    /// is compiled into this build, and a missing one would match nothing.
    #[test]
    fn parses_kotlin() {
        let rules = Rules::compile("id: t\nlanguage: kotlin\nrule:\n  kind: class_declaration\n")
            .expect("rules compile");
        // `.kts` is the same grammar: a build script is Kotlin whose top
        // level happens to be statements.
        for source in ["class Greeter { fun hi() {} }\n", "class Greeter\n"] {
            let tree = SourceTree::parse_kotlin(source);
            let found = tree.matches(&rules);
            assert_eq!(
                found.len(),
                1,
                "expected one class_declaration in {source:?}"
            );
            let (_, node) = &found[0];
            assert_eq!(node.kind(), "class_declaration");
            let name = node
                .children()
                .find(|c| c.kind() == "type_identifier")
                .expect("the declaration names a type");
            assert_eq!(name.text(), "Greeter");
        }
    }

    /// Bash, which [`one_match`] cannot check the same way for the second
    /// form: tree-sitter-bash names the `name` field on a
    /// `function_definition` written either way, but `function f { … }` and
    /// `f() { … }` are two spellings the grammar must both reach.
    #[test]
    fn parses_bash() {
        let yaml = "id: t\nlanguage: bash\nrule:\n  kind: function_definition\n";
        one_match(
            &SourceTree::parse_bash("hi() {\n  echo hi\n}\n"),
            yaml,
            "function_definition",
            "hi",
        );
        // The `function` keyword form, with and without parentheses.
        one_match(
            &SourceTree::parse_bash("function hi {\n  echo hi\n}\n"),
            yaml,
            "function_definition",
            "hi",
        );
    }

    /// Lua, which [`one_match`] cannot check the same way for every shape:
    /// tree-sitter-lua names a `name` field on a function declaration, but
    /// the name of `function M.foo()` is a `dot_index_expression` rather than
    /// an identifier. The point of the test is the same one every grammar
    /// check here makes — the grammar is compiled into this build at all.
    #[test]
    fn parses_lua() {
        one_match(
            &SourceTree::parse_lua("local function hi() end\n"),
            "id: t\nlanguage: lua\nrule:\n  kind: function_declaration\n",
            "function_declaration",
            "hi",
        );
    }

    /// HCL, which [`one_match`] cannot check: tree-sitter-hcl names no `name`
    /// field on a block — the block type and its labels are positional
    /// children — so the block type is the first `identifier` and every label
    /// after it is a `string_lit` or a bare `identifier`. The point of the
    /// test is the same as every other one here: the grammar is compiled into
    /// this build, and a missing one would match nothing.
    #[test]
    fn parses_hcl() {
        let rules =
            Rules::compile("id: t\nlanguage: hcl\nrule:\n  kind: block\n").expect("rules compile");
        let tree = SourceTree::parse_hcl("resource \"aws_vpc\" \"this\" {\n  cidr = 1\n}\n");
        let found = tree.matches(&rules);
        assert_eq!(found.len(), 1, "expected one block");
        let (_, node) = &found[0];
        assert_eq!(node.kind(), "block");
        let head: Vec<String> = node
            .children()
            .take_while(|c| c.kind() != "block_start")
            .map(|c| c.text().to_string())
            .collect();
        assert_eq!(head, ["resource", "\"aws_vpc\"", "\"this\""]);
    }

    #[test]
    fn parses_dart() {
        one_match(
            &SourceTree::parse_dart("class Greeter { void hi() {} }\n"),
            "id: t\nlanguage: dart\nrule:\n  kind: class_declaration\n",
            "class_declaration",
            "Greeter",
        );
    }

    /// YAML, which no track claims and one reads: Dart's `pubspec.yaml` is
    /// parsed with the grammar its own format has rather than with a line
    /// scanner, and this is what notices if that grammar ever stops being
    /// compiled in.
    #[test]
    fn parses_yaml() {
        let rules = Rules::compile("id: t\nlanguage: yaml\nrule:\n  kind: block_mapping_pair\n")
            .expect("rules compile");
        let tree = SourceTree::parse_yaml("name: collection\ndev_dependencies:\n  test: any\n");
        let found = tree.matches(&rules);
        assert_eq!(found.len(), 3, "two top-level pairs and one nested");
        let (_, node) = &found[0];
        assert_eq!(node.field("key").expect("a pair has a key").text(), "name");
        assert_eq!(
            node.field("value").expect("a pair has a value").text(),
            "collection",
        );
    }

    #[test]
    fn parses_scala() {
        one_match(
            &SourceTree::parse_scala("package p\nclass Greeter { def hi(): Unit = () }\n"),
            "id: t\nlanguage: scala\nrule:\n  kind: class_definition\n",
            "class_definition",
            "Greeter",
        );
    }

    #[test]
    fn parses_swift() {
        one_match(
            &SourceTree::parse_swift("class Greeter { func hi() {} }\n"),
            "id: t\nlanguage: swift\nrule:\n  kind: class_declaration\n",
            "class_declaration",
            "Greeter",
        );
    }

    #[test]
    fn parses_cpp() {
        one_match(
            &SourceTree::parse_cpp("namespace fmt { }\n"),
            "id: t\nlanguage: cpp\nrule:\n  kind: namespace_definition\n",
            "namespace_definition",
            "fmt",
        );
    }
}
