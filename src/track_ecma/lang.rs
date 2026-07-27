//! The two [`Language`] impls the EcmaScript track owns, and the per-file
//! header the extractor fills in and only the resolver reads.
//!
//! Two impls rather than one because the shared driver tags every stored row
//! with `L::LANG`, so a single impl could report only a single rate — and
//! this track owes two. Everything else is deliberately identical: the same
//! [`Language::DOMAIN`], the same `Header`, the same `Scope`, the same
//! `Config`. A `.ts` file importing a `.js` definition has to probe an
//! identity that can exist, and one domain is what makes that possible.

use crate::lang::Language;
use crate::model::{DeclSpace, Domain, Lang, Span};

/// Which grammar a file is parsed with.
///
/// Not a language and not a module kind: `.js`, `.mjs` and `.cjs` are one
/// grammar differing in module semantics ([`ModuleKind`]), and `.d.ts` is
/// plain TypeScript whose bodies happen to be absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// tree-sitter-javascript, which also covers JSX.
    JavaScript,
    /// tree-sitter-typescript.
    TypeScript,
}

/// A file's module semantics — NODE `ESM_FILE_FORMAT`.
///
/// Candidate generation depends on it: ESM does no extension probing and no
/// index resolution, CommonJS does both. Getting it wrong invents edges Node
/// would not create, or misses every one it would.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModuleKind {
    /// An ECMAScript module.
    Esm,
    /// A CommonJS module, evaluated inside Node's module wrapper.
    CommonJs,
    /// The file alone does not say. The nearest `package.json` `"type"` does,
    /// and that is a resolver input rather than a file fact.
    #[default]
    Undecided,
}

/// How [`ModuleKind`] was decided, so a guess is never mistaken for a fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModuleKindSource {
    /// `.mjs` or `.cjs`: the extension is normative.
    Extension,
    /// An `import`/`export` declaration, or a CommonJS export idiom. A
    /// parse-level marker, and a guess whenever both or neither appear —
    /// which is why it is recorded rather than folded into the kind.
    Syntax,
    /// Nothing in the file decided it.
    #[default]
    Undecided,
}

/// What an import binding takes from the module it names — ES `ImportEntry`
/// `[[ImportName]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportedName {
    /// The `default` export.
    Default,
    /// One named export. The string is the *exported* name, which is
    /// unrelated to the local one: `import { a as b }` is `Named("a")` bound
    /// to local `b`.
    Named(String),
    /// The module namespace object — `import * as ns`, `export * as ns`.
    Namespace,
    /// Every exported name — bare `export * from`.
    All,
    /// CommonJS `module.exports` as one value: `const m = require('m')`,
    /// `import m = require('m')`, and TypeScript's `export =`.
    Whole,
}

/// How a module was named at the site that named it.
///
/// The distinction is load-bearing for candidate generation: ESM `import`
/// and CommonJS `require` supply different condition sets to
/// `PACKAGE_TARGET_RESOLVE`, so one target package can resolve to two
/// different files from two importers in one repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSyntax {
    /// An `import`/`export … from` declaration.
    Esm,
    /// A `require(…)` call.
    Require,
    /// An `import(…)` expression.
    DynamicImport,
    /// TypeScript's `import x = require("m")`.
    ImportEquals,
    /// TypeScript's `import("./m").Foo` type node.
    ImportType,
}

/// One local name an import statement introduces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBinding {
    /// The local name bound in this file.
    pub local: String,
    /// What it takes from the named module.
    pub imported: ImportedName,
    /// Which declaration table the binding lands in. `import type` and an
    /// inline `type` modifier both give [`DeclSpace::Type`].
    pub space: DeclSpace,
}

/// One site that names a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleImport {
    /// The specifier when it is a string literal, unquoted. `None` when it is
    /// an arbitrary expression — the resolver's `DynamicModuleSpecifier`.
    pub specifier: Option<String>,
    /// The literal text at the site, whatever shape it had.
    pub raw_specifier: String,
    /// How the module was named.
    pub syntax: ImportSyntax,
    /// The local names introduced, in source order. Empty for a side-effect
    /// import, for a bare `require(…)` statement, and whenever the binding is
    /// not sound to record — a `let`/`var` CommonJS alias can be reassigned,
    /// so it binds nothing here rather than being guessed at.
    pub bindings: Vec<ImportBinding>,
    /// Where the site sits.
    pub span: Span,
}

/// One entry of this file's export map — ES `ExportEntry`.
///
/// A *fact about this file*, never a link. `export * from './x'` says only
/// that this module re-exports whatever `./x` exports; computing the name set
/// is `GetExportedNames`, it recurses into the module graph, and it is the
/// resolver's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportEntry {
    /// The exported name. `None` is the bare star, which exports every name
    /// of the requested module except `default`.
    pub export_name: Option<String>,
    /// The local declaration this exports, for a local export entry.
    pub local_name: Option<String>,
    /// The specifier for a re-export, unquoted. `None` for a local export.
    pub module_request: Option<String>,
    /// What is taken from the requested module, for a re-export.
    pub import_name: Option<ImportedName>,
    /// Which declaration table the export names. `export type { T }` and
    /// `export { type T }` both give [`DeclSpace::Type`].
    pub space: DeclSpace,
    /// Where the entry sits. Statement order decides the CommonJS cases —
    /// `exports.a = 1; module.exports = {}` exports nothing named `a` — so
    /// the span is a resolution input here, not only a report field.
    pub span: Span,
}

/// Per-file EcmaScript facts only the EcmaScript resolver reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EcmaHeader {
    /// Repo-relative, `/`-separated path of the file. A module's identity is
    /// its path, so this is the root of every FQN the file contributes.
    pub rel_path: String,
    /// The file's module semantics.
    pub module_kind: ModuleKind,
    /// How [`EcmaHeader::module_kind`] was decided.
    pub module_kind_source: ModuleKindSource,
    /// Whether the file is a Script rather than a Module: no top-level
    /// `import` or `export`. Its top-level declarations reach the global
    /// scope, and sloppy-mode hazards apply only here.
    pub script: bool,
    /// Every site that names a module, in rule order.
    pub imports: Vec<ModuleImport>,
    /// This file's export entries, in rule order.
    pub exports: Vec<ExportEntry>,
}

/// The EcmaScript resolver's per-file scope.
///
/// Empty until the resolver lands: the binding table it will hold is built
/// from [`EcmaHeader`] plus a probe, and building it is a linking step the
/// extractor is not allowed to take.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EcmaScope;

/// The EcmaScript resolver's project configuration.
///
/// Empty until the resolver lands. It will carry the `package.json` scopes,
/// the tsconfig chain's `baseUrl`/`paths`, and the condition set — none of
/// which any extractor may read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EcmaConfig;

/// JavaScript, as the shared driver sees it.
pub struct JsLang;

/// TypeScript, as the shared driver sees it.
pub struct TsLang;

/// Directory names a scan never descends into.
///
/// Build output duplicates the source graph: `dist/index.js` beside
/// `src/index.js` doubles every node, and one 3 MB minified bundle
/// contributes thousands of junk definitions that destroy the resolution
/// rate's meaning. `node_modules` is the dependency boundary — a specifier
/// resolving under it is `External`, so indexing it would move dependency
/// code into the rate.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    "out",
    "coverage",
    ".next",
    ".nuxt",
];

impl Language for JsLang {
    const LANG: Lang = Lang::JavaScript;
    const DOMAIN: Domain = Domain::EcmaScript;

    fn extensions() -> &'static [&'static str] {
        Lang::JavaScript.extensions()
    }

    fn skip_dirs() -> &'static [&'static str] {
        SKIP_DIRS
    }

    type Header = EcmaHeader;
    type Scope = EcmaScope;
    type Config = EcmaConfig;
}

impl Language for TsLang {
    const LANG: Lang = Lang::TypeScript;
    const DOMAIN: Domain = Domain::EcmaScript;

    fn extensions() -> &'static [&'static str] {
        Lang::TypeScript.extensions()
    }

    fn skip_dirs() -> &'static [&'static str] {
        SKIP_DIRS
    }

    type Header = EcmaHeader;
    type Scope = EcmaScope;
    type Config = EcmaConfig;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_languages_one_identity_space() {
        // One domain, because a `.ts` import may resolve to a `.js`
        // definition and the probe has to be able to hit.
        assert_eq!(<JsLang as Language>::DOMAIN, <TsLang as Language>::DOMAIN);
        assert_eq!(<JsLang as Language>::DOMAIN, Domain::EcmaScript);
        // Two `Lang`s, because a rate is per language and never aggregated.
        assert_ne!(<JsLang as Language>::LANG, <TsLang as Language>::LANG);
    }

    #[test]
    fn each_impl_owns_exactly_its_languages_extensions() {
        assert_eq!(<JsLang as Language>::extensions(), ["js", "mjs", "cjs"]);
        assert_eq!(<TsLang as Language>::extensions(), ["ts"]);
        // The registry's view and the impl's view are one list; two sources
        // of truth here would mean a walk reading a file nobody owns.
        assert_eq!(
            <JsLang as Language>::extensions(),
            Lang::JavaScript.extensions()
        );
        assert_eq!(
            <TsLang as Language>::extensions(),
            Lang::TypeScript.extensions()
        );
    }

    #[test]
    fn build_output_is_skipped_by_both() {
        for dir in ["node_modules", "dist", "coverage"] {
            assert!(<JsLang as Language>::skip_dirs().contains(&dir));
            assert!(<TsLang as Language>::skip_dirs().contains(&dir));
        }
    }
}
