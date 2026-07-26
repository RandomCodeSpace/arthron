//! The Go resolver: all cross-file linking for Go. Never drops.
//!
//! Probes a [`SymbolProbe`] (the store in production, a `HashSet` in tests)
//! with content-addressed candidate FQNs and classifies every reference
//! into exactly one [`Outcome`]. Every candidate probed — hit or miss — is
//! returned for the invalidation index.

use std::collections::HashMap;

use crate::model::{DefKind, Definition, Lang, NodeId, RefTarget, Reference, node_id};
use crate::extract_go::FileFacts;
use crate::{Outcome, UnresolvedReason};

/// Go's universe-scope builtin functions.
const GO_BUILTINS: &[&str] = &[
    "append", "cap", "clear", "close", "complex", "copy", "delete", "imag",
    "len", "make", "max", "min", "new", "panic", "print", "println",
    "real", "recover",
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
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("module ") {
            path = Some(rest.trim().to_string());
        } else if line == "require (" {
            in_require_block = true;
        } else if in_require_block && line == ")" {
            in_require_block = false;
        } else if in_require_block {
            if let Some(dep) = line.split_whitespace().next() {
                requires.push(dep.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("require ") {
            if let Some(dep) = rest.split_whitespace().next() {
                requires.push(dep.to_string());
            }
        }
    }
    Some(GoModule { path: path?, requires })
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

/// Build a [`FileScope`] from a file's extracted facts.
pub fn file_scope(module: &GoModule, rel_dir: &str, facts: &FileFacts) -> FileScope {
    let resolver = GoResolver { module: module.clone() };
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
                let last = imp.path.rsplit('/').next().unwrap_or(&imp.path);
                imports.insert(last.to_string(), imp.path.clone());
            }
        }
    }
    FileScope { pkg_path: resolver.package_path(rel_dir), imports, dot_imports }
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
            return Resolution { outcome, candidates: vec![id] };
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
                if GO_BUILTINS.contains(&name.as_str()) {
                    return Resolution {
                        outcome: Outcome::External("go:builtin".to_string()),
                        candidates: vec![],
                    };
                }
                // Same package first, then internal dot-imports, in order.
                let mut candidates =
                    vec![node_id(Lang::Go, &format!("{}.{name}", scope.pkg_path))];
                for dot in &scope.dot_imports {
                    if self.is_internal(dot) {
                        candidates.push(node_id(Lang::Go, &format!("{dot}.{name}")));
                    }
                }
                for id in &candidates {
                    if probe.contains(id) {
                        return Resolution {
                            outcome: Outcome::Resolved(*id),
                            candidates,
                        };
                    }
                }
                Resolution {
                    outcome: Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
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
                        Resolution { outcome, candidates: vec![id] }
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
            span: Span { byte_start: 0, byte_end: 0, line: 1 },
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
        let miss = r.resolve_call(&call(RefTarget::Plain { name: "helper".into() }), &scope(), &table);
        assert_eq!(miss.outcome, Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition));
        assert_eq!(miss.candidates, vec![helper]); // the miss is recorded
        table.insert(helper);
        let hit = r.resolve_call(&call(RefTarget::Plain { name: "helper".into() }), &scope(), &table);
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
    fn qualified_calls_classify_by_import_table() {
        let r = GoResolver { module: module() };
        let target = node_id(Lang::Go, "example.com/app/util.Parse");
        let mut table = HashSet::new();
        table.insert(target);
        let internal = r.resolve_call(
            &call(RefTarget::Qualified { qualifier: "util".into(), name: "Parse".into() }),
            &scope(),
            &table,
        );
        assert_eq!(internal.outcome, Outcome::Resolved(target));
        let external = r.resolve_call(
            &call(RefTarget::Qualified { qualifier: "errors".into(), name: "Wrap".into() }),
            &scope(),
            &table,
        );
        assert_eq!(external.outcome, Outcome::External("github.com/pkg/errors".into()));
        let variable = r.resolve_call(
            &call(RefTarget::Qualified { qualifier: "conn".into(), name: "Close".into() }),
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
