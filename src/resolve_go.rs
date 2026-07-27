//! The Go resolver: all cross-file linking for Go. Never drops.
//!
//! Probes a [`SymbolProbe`] (the store in production, a `HashSet` in tests)
//! with content-addressed candidate FQNs and classifies every reference
//! into exactly one [`Outcome`]. Every candidate probed — hit or miss — is
//! returned for the invalidation index.

use std::collections::HashMap;

use crate::extract_go::FileFacts;
use crate::model::{DefKind, Definition, Lang, NodeId, RefTarget, Reference, node_id};
use crate::{Outcome, UnresolvedReason};

/// Go's universe-scope builtin functions.
const GO_BUILTINS: &[&str] = &[
    "append", "cap", "clear", "close", "complex", "copy", "delete", "imag", "len", "make", "max",
    "min", "new", "panic", "print", "println", "real", "recover",
];

/// Facts parsed from a `go.mod` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoModule {
    /// The module path from the `module` directive.
    pub path: String,
    /// Module paths from `require` directives.
    pub requires: Vec<String>,
}

/// Parse a `go.mod` source. `None` when there is no `module` directive.
pub fn parse_go_mod(src: &str) -> Option<GoModule> {
    let mut path = None;
    let mut requires = Vec::new();
    let mut in_require_block = false;
    for line in src.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        // go.mod separates a directive from its argument with any run of
        // whitespace — gofmt writes tabs — so tokenise rather than match a
        // single-space prefix.
        let mut tokens = line.split_whitespace();
        let Some(first) = tokens.next() else {
            continue; // blank, or comment-only
        };
        let second = tokens.next();
        if in_require_block {
            if first == ")" {
                in_require_block = false;
            } else {
                requires.push(first.to_string());
            }
            continue;
        }
        match (first, second) {
            ("module", Some(module_path)) => path = Some(module_path.to_string()),
            ("require", Some("(")) => in_require_block = true,
            ("require", Some(dep)) => requires.push(dep.to_string()),
            _ => {}
        }
    }
    Some(GoModule {
        path: path?,
        requires,
    })
}

/// The name of Go's package initialiser, `func init()`.
///
/// Go forbids naming one: it cannot be called, assigned, or referred to at
/// all, and a package may declare any number of them. A node is a thing a
/// reference can name, so this is not one — it gets no definition node, and
/// the references inside it belong to the package, exactly as a package-level
/// variable's initialiser does. Giving it a node instead collapses every
/// `init` in a package into a single `{pkg}.init` identity, which is one
/// definition nothing can name standing in for many distinct bodies.
pub const INIT_FUNC: &str = "init";

/// Whether a definition is Go's package initialiser. See [`INIT_FUNC`].
///
/// A *method* called `init` is an ordinary method — `x.init()` names it — so
/// only a plain function declaration qualifies.
pub fn is_init_func(def: &Definition) -> bool {
    matches!(def.kind, DefKind::Function) && def.name == INIT_FUNC
}

/// The resolver's view of the symbol table: one membership probe per
/// candidate. The store implements this; tests use a `HashSet`.
pub trait SymbolProbe {
    /// Whether a node with this identity exists in the graph.
    fn contains(&self, id: &NodeId) -> bool;
}

impl SymbolProbe for std::collections::HashSet<NodeId> {
    fn contains(&self, id: &NodeId) -> bool {
        std::collections::HashSet::contains(self, id)
    }
}

/// One classified reference plus every candidate probed on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The single outcome. There is no way to express "dropped".
    pub outcome: Outcome<NodeId, String>,
    /// Every candidate FQN hash probed, hit or miss, in probe order.
    /// Feeds the candidate-set invalidation index.
    pub candidates: Vec<NodeId>,
}

/// Per-file resolution scope: the package path plus the import table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileScope {
    /// Import path of the package this file belongs to.
    pub pkg_path: String,
    /// Import name (alias or last path segment) → import path.
    /// Blank (`_`) and dot (`.`) imports are excluded.
    pub imports: HashMap<String, String>,
    /// Paths dot-imported into this file's scope.
    pub dot_imports: Vec<String>,
}

/// Whether a path segment is a Go major-version marker (`v2`, `v3`, …).
fn is_version_segment(segment: &str) -> bool {
    match segment.strip_prefix('v') {
        Some(digits) => !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// The name an unaliased import path *suggests* it binds to.
///
/// Go binds an unaliased import to the imported package's **declared** name,
/// which lives in that package's source and not in the import path. This is
/// therefore a heuristic, and the answer only when the declaration is out of
/// reach: an external package, whose source is never indexed, or a package in
/// this repository that no scan has reached yet. Module version suffixes are
/// how the last segment usually lies, so strip them — `gopkg.in/yaml.v3`
/// binds `yaml`, `github.com/foo/bar/v2` binds `bar`, and a plain path binds
/// its last segment.
///
/// For a package in this repository the declared name is a fact the extractor
/// already records (`FileFacts::package`); [`file_scope`] prefers it, given
/// the pipeline's `package_names` map. It doubles as the name a directory's
/// package is assumed to use when nothing in it has been indexed.
pub fn import_binding(path: &str) -> &str {
    let mut segments = path.rsplit('/');
    let last = segments.next().unwrap_or(path);
    if is_version_segment(last) {
        // `github.com/foo/bar/v2` → `bar`; a bare `v2` has nothing before it.
        return segments.next().filter(|s| !s.is_empty()).unwrap_or(last);
    }
    match last.rsplit_once('.') {
        // `gopkg.in/yaml.v3` → `yaml`
        Some((name, version)) if !name.is_empty() && is_version_segment(version) => name,
        _ => last,
    }
}

/// Whether a declared package name marks an *external test package*.
///
/// `package foo_test` in the directory of package `foo` is a second package
/// sharing one directory — Go's only such case. `dir_pkg_name` is the name
/// that directory's package is known to use, and the comparison is what
/// keeps a directory whose package genuinely is called `foo_test` out of
/// this: there the declared name is the directory's own name.
pub fn is_external_test_package(declared: &str, dir_pkg_name: &str) -> bool {
    declared.ends_with("_test") && declared != dir_pkg_name
}

/// The import path a file's definitions and same-package candidates live
/// under: its directory's package path, unless it is an external test file.
///
/// An external test package may not be imported by anything, and its
/// definitions must not sit in the production package's namespace, where a
/// same-package candidate from a production file would wrongly hit one. It
/// gets `{dir_pkg_path}_test` — its own package, with its own node. It
/// reaches the package under test the ordinary way, through the explicit
/// import it has to write anyway.
///
/// One ambiguity survives, knowingly: a directory literally named `foo_test`
/// next to the external test package of `foo` claims the same path, so their
/// definitions share a namespace. Go permits that pair and neither package is
/// importable, so no import can be misread — but a same-package candidate
/// could cross between them. Recorded rather than papered over; a separator
/// no import path can contain would cost every FQN its readability.
pub fn package_path_for_file(
    dir_pkg_path: &str,
    declared: Option<&str>,
    dir_pkg_name: &str,
) -> String {
    match declared {
        Some(name) if is_external_test_package(name, dir_pkg_name) => {
            format!("{dir_pkg_path}_test")
        }
        _ => dir_pkg_path.to_string(),
    }
}

/// Build a [`FileScope`] from a file's extracted facts.
///
/// `pkg_path` is the package the file's own definitions live in — the
/// pipeline computes it, because [`package_path_for_file`] needs to know
/// what name the directory's package uses.
///
/// `package_names` maps import path → declared package name for every
/// package this repository has indexed. It too belongs to the pipeline:
/// binding an unaliased import needs a fact out of the *imported* package's
/// source, and the pipeline is the only layer that sees every package.
/// Anything missing from it falls back to [`import_binding`].
pub fn file_scope(
    resolver: &GoResolver,
    pkg_path: String,
    facts: &FileFacts,
    package_names: &HashMap<String, String>,
) -> FileScope {
    let mut imports = HashMap::new();
    let mut dot_imports = Vec::new();
    for imp in &facts.imports {
        match imp.alias.as_deref() {
            Some("_") => {}
            Some(".") => dot_imports.push(imp.path.clone()),
            Some(alias) => {
                imports.insert(alias.to_string(), imp.path.clone());
            }
            None => {
                let declared = package_names
                    .get(&imp.path)
                    .filter(|_| resolver.is_internal(&imp.path))
                    .map(String::as_str);
                let bound = declared.unwrap_or_else(|| import_binding(&imp.path));
                imports.insert(bound.to_string(), imp.path.clone());
            }
        }
    }
    FileScope {
        pkg_path,
        imports,
        dot_imports,
    }
}

/// All Go linking decisions. Owns the module facts; sees every file's scope.
#[derive(Debug, Clone)]
pub struct GoResolver {
    /// The module this repository declares.
    pub module: GoModule,
}

impl GoResolver {
    /// The import path of the package in a directory (repo-relative, `/`
    /// separated, empty string for the module root).
    pub fn package_path(&self, rel_dir: &str) -> String {
        if rel_dir.is_empty() {
            self.module.path.clone()
        } else {
            format!("{}/{}", self.module.path, rel_dir)
        }
    }

    /// Canonical FQN for a definition in a package.
    pub fn def_fqn(pkg_path: &str, def: &Definition) -> String {
        match (&def.kind, &def.receiver) {
            (DefKind::Method, Some(recv)) => {
                format!("{pkg_path}.{recv}.{}", def.name)
            }
            _ => format!("{pkg_path}.{}", def.name),
        }
    }

    fn is_internal(&self, import_path: &str) -> bool {
        import_path == self.module.path
            || import_path.starts_with(&format!("{}/", self.module.path))
    }

    fn is_stdlib(import_path: &str) -> bool {
        !import_path
            .split('/')
            .next()
            .unwrap_or(import_path)
            .contains('.')
    }

    /// Classify an import reference.
    pub fn resolve_import(&self, path: &str, probe: &dyn SymbolProbe) -> Resolution {
        if self.is_internal(path) {
            let id = node_id(Lang::Go, path);
            let outcome = if probe.contains(&id) {
                Outcome::Resolved(id)
            } else {
                Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
            };
            return Resolution {
                outcome,
                candidates: vec![id],
            };
        }
        if Self::is_stdlib(path) {
            return Resolution {
                outcome: Outcome::External(format!("std:{path}")),
                candidates: vec![],
            };
        }
        if self
            .module
            .requires
            .iter()
            .any(|r| path == r || path.starts_with(&format!("{r}/")))
        {
            return Resolution {
                outcome: Outcome::External(path.to_string()),
                candidates: vec![],
            };
        }
        Resolution {
            outcome: Outcome::Unresolved(UnresolvedReason::UnknownPackage),
            candidates: vec![],
        }
    }

    /// Classify a call reference against a file's scope.
    pub fn resolve_call(
        &self,
        r: &Reference,
        scope: &FileScope,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        match &r.target {
            RefTarget::Plain { name } => {
                // Same package first, then internal dot-imports, in order.
                // Generated and probed one at a time, stopping at the first
                // hit: `candidates` must list what was probed and nothing
                // else, or the invalidation index it feeds would wake this
                // reference for edits that could not change its outcome.
                let same_pkg = format!("{}.{name}", scope.pkg_path);
                let dotted = scope
                    .dot_imports
                    .iter()
                    .filter(|dot| self.is_internal(dot))
                    .map(|dot| format!("{dot}.{name}"));
                let mut candidates = Vec::new();
                for fqn in std::iter::once(same_pkg).chain(dotted) {
                    let id = node_id(Lang::Go, &fqn);
                    candidates.push(id);
                    if probe.contains(&id) {
                        return Resolution {
                            outcome: Outcome::Resolved(id),
                            candidates,
                        };
                    }
                }
                // Nothing in scope defines the name. The universe scope is
                // the outermost one, so a builtin is the answer only after
                // every candidate has been probed and missed — otherwise a
                // package-level `min` could never resolve.
                let outcome = if GO_BUILTINS.contains(&name.as_str()) {
                    Outcome::External("go:builtin".to_string())
                } else {
                    Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
                };
                Resolution {
                    outcome,
                    candidates,
                }
            }
            RefTarget::Qualified { qualifier, name } => {
                match scope.imports.get(qualifier) {
                    Some(path) if self.is_internal(path) => {
                        let id = node_id(Lang::Go, &format!("{path}.{name}"));
                        let outcome = if probe.contains(&id) {
                            Outcome::Resolved(id)
                        } else {
                            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
                        };
                        Resolution {
                            outcome,
                            candidates: vec![id],
                        }
                    }
                    Some(path) => Resolution {
                        outcome: Outcome::External(path.clone()),
                        candidates: vec![],
                    },
                    // The qualifier is a variable: needs type inference.
                    None => Resolution {
                        outcome: Outcome::Unresolved(UnresolvedReason::NeedsTypeInference),
                        candidates: vec![],
                    },
                }
            }
            RefTarget::Complex => Resolution {
                outcome: Outcome::Unresolved(UnresolvedReason::NeedsTypeInference),
                candidates: vec![],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract_go::Import;
    use crate::model::{RefKind, Span};
    use std::collections::HashSet;

    fn module() -> GoModule {
        GoModule {
            path: "example.com/app".into(),
            requires: vec!["github.com/pkg/errors".into()],
        }
    }

    fn call(target: RefTarget) -> Reference {
        Reference {
            kind: RefKind::Call,
            raw_target: String::new(),
            target,
            enclosing: None,
            span: Span {
                byte_start: 0,
                byte_end: 0,
                line: 1,
            },
        }
    }

    fn scope() -> FileScope {
        let mut imports = HashMap::new();
        imports.insert("util".into(), "example.com/app/util".into());
        imports.insert("errors".into(), "github.com/pkg/errors".into());
        FileScope {
            pkg_path: "example.com/app/server".into(),
            imports,
            dot_imports: vec![],
        }
    }

    #[test]
    fn go_mod_parses_module_and_requires() {
        let m = parse_go_mod(
            "module example.com/app\n\ngo 1.22\n\nrequire (\n\tgithub.com/pkg/errors v0.9.1 // indirect\n)\nrequire golang.org/x/sync v0.7.0\n",
        )
        .unwrap();
        assert_eq!(m.path, "example.com/app");
        assert_eq!(m.requires, ["github.com/pkg/errors", "golang.org/x/sync"]);
    }

    #[test]
    fn go_mod_directives_tolerate_tabs_and_runs_of_spaces() {
        // gofmt aligns go.mod with tabs, so "module\tpath" is the common
        // on-disk shape, not the exception.
        let spaced = parse_go_mod(
            "module example.com/app\n\ngo 1.22\n\nrequire (\n\tgithub.com/pkg/errors v0.9.1 // indirect\n)\nrequire golang.org/x/sync v0.7.0\n",
        )
        .expect("spaced go.mod parses");
        let tabbed = parse_go_mod(
            "module\texample.com/app\n\ngo 1.22\n\nrequire\t(\n\tgithub.com/pkg/errors\tv0.9.1 // indirect\n)\nrequire   golang.org/x/sync v0.7.0\n",
        )
        .expect("tab-separated go.mod parses");
        assert_eq!(tabbed, spaced);
    }

    #[test]
    fn init_is_not_a_node_but_a_method_called_init_is() {
        let def = |kind, name: &str, receiver: Option<&str>| Definition {
            kind,
            name: name.to_string(),
            receiver: receiver.map(str::to_string),
            span: Span {
                byte_start: 0,
                byte_end: 0,
                line: 1,
            },
        };
        assert!(is_init_func(&def(DefKind::Function, "init", None)));
        assert!(
            !is_init_func(&def(DefKind::Method, "init", Some("Conn"))),
            "`c.init()` names a method, so the method is a node"
        );
        assert!(!is_init_func(&def(DefKind::Function, "initialise", None)));
    }

    #[test]
    fn an_external_test_package_is_a_package_of_its_own() {
        // `package graph_test` in the directory of package `graph`.
        assert_eq!(
            package_path_for_file("example.com/app/graph", Some("graph_test"), "graph"),
            "example.com/app/graph_test"
        );
        // An in-package test file is the production package.
        assert_eq!(
            package_path_for_file("example.com/app/graph", Some("graph"), "graph"),
            "example.com/app/graph"
        );
        // A directory whose package genuinely is called `foo_test` is not a
        // test package: the name it declares is the name it uses.
        assert_eq!(
            package_path_for_file("example.com/app/foo_test", Some("foo_test"), "foo_test"),
            "example.com/app/foo_test"
        );
        // No package clause parsed: the directory's package path stands.
        assert_eq!(
            package_path_for_file("example.com/app/graph", None, "graph"),
            "example.com/app/graph"
        );
    }

    #[test]
    fn internal_import_resolves_against_the_probe() {
        let r = GoResolver { module: module() };
        let pkg = node_id(Lang::Go, "example.com/app/util");
        let mut table = HashSet::new();
        assert_eq!(
            r.resolve_import("example.com/app/util", &table).outcome,
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
        );
        table.insert(pkg);
        assert_eq!(
            r.resolve_import("example.com/app/util", &table).outcome,
            Outcome::Resolved(pkg)
        );
    }

    #[test]
    fn stdlib_known_dep_and_unknown_imports_classify() {
        let r = GoResolver { module: module() };
        let t: HashSet<NodeId> = HashSet::new();
        assert_eq!(
            r.resolve_import("net/http", &t).outcome,
            Outcome::External("std:net/http".into())
        );
        assert_eq!(
            r.resolve_import("github.com/pkg/errors", &t).outcome,
            Outcome::External("github.com/pkg/errors".into())
        );
        assert_eq!(
            r.resolve_import("github.com/nobody/mystery", &t).outcome,
            Outcome::Unresolved(UnresolvedReason::UnknownPackage)
        );
    }

    #[test]
    fn plain_call_probes_same_package() {
        let r = GoResolver { module: module() };
        let helper = node_id(Lang::Go, "example.com/app/server.helper");
        let mut table = HashSet::new();
        let miss = r.resolve_call(
            &call(RefTarget::Plain {
                name: "helper".into(),
            }),
            &scope(),
            &table,
        );
        assert_eq!(
            miss.outcome,
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
        );
        assert_eq!(miss.candidates, vec![helper]); // the miss is recorded
        table.insert(helper);
        let hit = r.resolve_call(
            &call(RefTarget::Plain {
                name: "helper".into(),
            }),
            &scope(),
            &table,
        );
        assert_eq!(hit.outcome, Outcome::Resolved(helper));
    }

    #[test]
    fn builtins_are_external() {
        let r = GoResolver { module: module() };
        let t: HashSet<NodeId> = HashSet::new();
        let res = r.resolve_call(&call(RefTarget::Plain { name: "len".into() }), &scope(), &t);
        assert_eq!(res.outcome, Outcome::External("go:builtin".into()));
    }

    #[test]
    fn unaliased_imports_bind_without_module_version_suffixes() {
        let span = Span {
            byte_start: 0,
            byte_end: 0,
            line: 1,
        };
        let unaliased = |path: &str| Import {
            alias: None,
            path: path.to_string(),
            span,
        };
        let facts = FileFacts {
            imports: vec![
                unaliased("gopkg.in/yaml.v3"),
                unaliased("github.com/foo/bar/v2"),
                unaliased("example.com/plain"),
            ],
            ..FileFacts::default()
        };
        let r = GoResolver { module: module() };
        let s = file_scope(
            &r,
            r.package_path("server"),
            &facts,
            &HashMap::new(), // nothing indexed: every binding is the heuristic
        );
        assert_eq!(
            s.imports.get("yaml").map(String::as_str),
            Some("gopkg.in/yaml.v3")
        );
        assert_eq!(
            s.imports.get("bar").map(String::as_str),
            Some("github.com/foo/bar/v2")
        );
        assert_eq!(
            s.imports.get("plain").map(String::as_str),
            Some("example.com/plain")
        );
        assert_eq!(s.imports.len(), 3);

        // The symptom the binding name causes: an unknown qualifier is read
        // as a variable, so every `yaml.X()` call would need type inference.
        let r = GoResolver { module: module() };
        let t: HashSet<NodeId> = HashSet::new();
        let res = r.resolve_call(
            &call(RefTarget::Qualified {
                qualifier: "yaml".into(),
                name: "Unmarshal".into(),
            }),
            &s,
            &t,
        );
        assert_eq!(res.outcome, Outcome::External("gopkg.in/yaml.v3".into()));
    }

    #[test]
    fn a_known_internal_package_binds_its_declared_name_not_its_directory() {
        let span = Span {
            byte_start: 0,
            byte_end: 0,
            line: 1,
        };
        let unaliased = |path: &str| Import {
            alias: None,
            path: path.to_string(),
            span,
        };
        let facts = FileFacts {
            imports: vec![
                unaliased("example.com/app/utilx"), // declares `package util`
                unaliased("example.com/app/other"), // not indexed yet
                unaliased("gopkg.in/yaml.v3"),      // external: never indexed
            ],
            ..FileFacts::default()
        };
        let mut names = HashMap::new();
        names.insert("example.com/app/utilx".to_string(), "util".to_string());
        let r = GoResolver { module: module() };
        let s = file_scope(&r, r.package_path("server"), &facts, &names);

        assert_eq!(
            s.imports.get("util").map(String::as_str),
            Some("example.com/app/utilx"),
            "the declared name is the binding"
        );
        assert!(
            !s.imports.contains_key("utilx"),
            "the directory name is not a binding: {:?}",
            s.imports
        );
        // Nothing known about the package: the path is the only evidence.
        assert_eq!(
            s.imports.get("other").map(String::as_str),
            Some("example.com/app/other")
        );
        assert_eq!(
            s.imports.get("yaml").map(String::as_str),
            Some("gopkg.in/yaml.v3")
        );
    }

    #[test]
    fn candidates_record_exactly_what_was_probed() {
        // `candidates` feeds the invalidation index: a candidate listed but
        // never probed would wake this reference for an edit that could not
        // have changed its outcome.
        let r = GoResolver { module: module() };
        let same_pkg = node_id(Lang::Go, "example.com/app/server.helper");
        let dot = node_id(Lang::Go, "example.com/app/util.helper");
        let mut s = scope();
        s.dot_imports.push("example.com/app/util".into());

        let mut table = HashSet::new();
        let miss = r.resolve_call(
            &call(RefTarget::Plain {
                name: "helper".into(),
            }),
            &s,
            &table,
        );
        // A total miss probes every candidate, so it records every candidate.
        assert_eq!(miss.candidates, vec![same_pkg, dot]);

        table.insert(same_pkg);
        let hit = r.resolve_call(
            &call(RefTarget::Plain {
                name: "helper".into(),
            }),
            &s,
            &table,
        );
        assert_eq!(hit.outcome, Outcome::Resolved(same_pkg));
        assert_eq!(hit.candidates.len(), 1, "the dot-import was never probed");
        assert_eq!(hit.candidates, vec![same_pkg]);
    }

    #[test]
    fn a_package_level_definition_beats_the_builtin_of_the_same_name() {
        // Go's universe scope is the outermost one: a package-level `min`
        // shadows the builtin, so the builtin answer is only correct once
        // every candidate in scope has been probed and missed.
        let r = GoResolver { module: module() };
        let min = node_id(Lang::Go, "example.com/app/server.min");
        let mut table = HashSet::new();
        let builtin = r.resolve_call(
            &call(RefTarget::Plain { name: "min".into() }),
            &scope(),
            &table,
        );
        assert_eq!(builtin.outcome, Outcome::External("go:builtin".into()));
        assert_eq!(builtin.candidates, vec![min]); // the miss is still recorded
        table.insert(min);
        let shadowed = r.resolve_call(
            &call(RefTarget::Plain { name: "min".into() }),
            &scope(),
            &table,
        );
        assert_eq!(shadowed.outcome, Outcome::Resolved(min));
        assert_eq!(shadowed.candidates, vec![min]);
    }

    #[test]
    fn qualified_calls_classify_by_import_table() {
        let r = GoResolver { module: module() };
        let target = node_id(Lang::Go, "example.com/app/util.Parse");
        let mut table = HashSet::new();
        table.insert(target);
        let internal = r.resolve_call(
            &call(RefTarget::Qualified {
                qualifier: "util".into(),
                name: "Parse".into(),
            }),
            &scope(),
            &table,
        );
        assert_eq!(internal.outcome, Outcome::Resolved(target));
        let external = r.resolve_call(
            &call(RefTarget::Qualified {
                qualifier: "errors".into(),
                name: "Wrap".into(),
            }),
            &scope(),
            &table,
        );
        assert_eq!(
            external.outcome,
            Outcome::External("github.com/pkg/errors".into())
        );
        let variable = r.resolve_call(
            &call(RefTarget::Qualified {
                qualifier: "conn".into(),
                name: "Close".into(),
            }),
            &scope(),
            &table,
        );
        assert_eq!(
            variable.outcome,
            Outcome::Unresolved(UnresolvedReason::NeedsTypeInference)
        );
    }

    #[test]
    fn complex_targets_need_type_inference() {
        let r = GoResolver { module: module() };
        let t: HashSet<NodeId> = HashSet::new();
        let res = r.resolve_call(&call(RefTarget::Complex), &scope(), &t);
        assert_eq!(
            res.outcome,
            Outcome::Unresolved(UnresolvedReason::NeedsTypeInference)
        );
    }
}
