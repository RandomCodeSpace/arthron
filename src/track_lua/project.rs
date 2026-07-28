//! Phase 0 for Lua: the one place in a repository that maps a module name to
//! a file.
//!
//! Lua states it nowhere in its source. `require 'busted.core'` hands a
//! string to a function, which searches `package.path` — a runtime value,
//! rewritten by the very corpus this track is measured against
//! (`busted/runner.lua` prepends the `--lpath` argument to it before anything
//! is required). A rockspec's `build.modules` table is the one place that
//! states the mapping as a fact:
//!
//! ```lua
//! build = { modules = { ['busted.core'] = 'busted/core.lua' } }
//! ```
//!
//! That table is to Lua what the `module` directive is to Go: it roots the
//! identity of every module the repository ships, which is why it is
//! fingerprinted by [`crate::lang::Resolver::config_digest`] and why a scan
//! of the same tree under a different one is a different graph.
//!
//! # What is read, and what is not
//!
//! `*.rockspec` at the repository root and under `rockspecs/`, which are the
//! two places LuaRocks puts one. Only `build.modules`, in either of the two
//! spellings a rockspec may use for it.
//!
//! **`dependencies` is deliberately not read, and that is a measured call,
//! not an omission.** A rockspec declares *rock* names, and a rock name is
//! not a module name. The measured corpus declares nine and disproves the
//! identification six times: `penlight` ships `pl.*`, `lua-term` ships
//! `term`, `lua_cliargs` ships `cliargs`, `mediator_lua` ships `mediator`,
//! `luasystem` ships `system`, and `lua` ships the standard library. Three
//! more — `say`, `dkjson`, `luassert` — happen to coincide, and reading the
//! coincidence as a rule would mint [`crate::Outcome::External`] from a
//! convention the same manifest refutes in the majority. `External` sits
//! outside *both* terms of the resolution rate, so it is the cheapest bucket
//! there is to inflate; this track mints none. See
//! [`crate::track_lua::resolve`].
//!
//! A rockspec is Lua source, and it is parsed as Lua rather than pattern
//! matched: `['busted.core'] = 'busted/core.lua'` is a table entry whose key
//! and value are literals, and reading it as one is both simpler and less
//! wrong than a regular expression over the same bytes.
//!
//! No file is executed and no network call is made, here or anywhere: a
//! rockspec that computes its module map — the corpus's own computes its
//! `version` and its `source.url` by concatenation — contributes the literals
//! it states and nothing else.

use std::collections::BTreeMap;
use std::path::Path;

use crate::lang::LayoutError;
use crate::sg::{Rules, SgNode, SourceTree};
use crate::track_lua::extract::{field_name, field_nodes, string_literal};

/// The directory beside the repository root that LuaRocks keeps released
/// rockspecs in.
const ROCKSPEC_DIR: &str = "rockspecs";

/// What the project's manifests decide for every Lua reference in the tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LuaProject {
    /// Module name → repo-relative file path, from every `build.modules`
    /// table phase 0 read. The first manifest to state a name keeps it, so
    /// the answer does not depend on directory order.
    pub modules: BTreeMap<String, String>,
    /// The rockspecs phase 0 read, repo-relative and sorted. Provenance for
    /// the digest, so that adding a manifest re-roots the graph rather than
    /// silently widening it.
    pub rockspecs: Vec<String>,
}

impl LuaProject {
    /// The file a manifest says a module name is, if one says so.
    pub fn declared_module(&self, name: &str) -> Option<&str> {
        self.modules.get(name).map(String::as_str)
    }

    /// A stable fingerprint of everything phase 0 read.
    pub fn digest(&self) -> Vec<u8> {
        let mut out = String::new();
        for (name, path) in &self.modules {
            out.push_str(name);
            out.push('\u{2}');
            out.push_str(path);
            out.push('\n');
        }
        out.push('\u{1}');
        for spec in &self.rockspecs {
            out.push_str(spec);
            out.push('\n');
        }
        out.into_bytes()
    }
}

/// Read the project's manifests from the tree at `root`.
///
/// Never fails on a repository that declares nothing: a Lua tree with no
/// rockspec is an ordinary shape — a script directory, an application, a
/// plugin — and the resolver then has only `package.path`'s own patterns to
/// work with, which is exactly what it says when it misses.
pub fn layout(root: &Path) -> Result<LuaProject, LayoutError> {
    let mut manifests: Vec<String> = Vec::new();
    for dir in ["", ROCKSPEC_DIR] {
        let here = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(dir)
        };
        let entries = match std::fs::read_dir(&here) {
            Ok(entries) => entries,
            // The repository root must be readable; `rockspecs/` need not
            // exist, and a repository without one is the common shape.
            Err(e) if dir.is_empty() => {
                return Err(LayoutError {
                    message: format!("reading {}: {e}", here.display()),
                });
            }
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("rockspec")
                && path.is_file()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                manifests.push(if dir.is_empty() {
                    name.to_string()
                } else {
                    format!("{dir}/{name}")
                });
            }
        }
    }
    // Directory order is whatever the filesystem says; the read order must
    // not be.
    manifests.sort();

    let mut project = LuaProject::default();
    for name in &manifests {
        // A rockspec that cannot be read as UTF-8 is not one this build can
        // learn anything from, and refusing the whole scan over one would
        // turn a manifest oddity into a missing measurement.
        let Ok(source) = std::fs::read_to_string(root.join(name)) else {
            continue;
        };
        project.rockspecs.push(name.clone());
        for (module, file) in read_rockspec(&source) {
            project.modules.entry(module).or_insert(file);
        }
    }
    Ok(project)
}

/// The `build.modules` map one rockspec states, in either spelling.
fn read_rockspec(source: &str) -> Vec<(String, String)> {
    static RULES: std::sync::OnceLock<Rules> = std::sync::OnceLock::new();
    let rules = RULES.get_or_init(|| {
        Rules::compile("id: assign\nlanguage: lua\nrule:\n  kind: assignment_statement\n")
            .expect("the rockspec rule compiles")
    });

    let mut out = Vec::new();
    let tree = SourceTree::parse_lua(source);
    for (_, node) in tree.matches(rules) {
        for (var, val) in pairs(&node) {
            if val.kind() != "table_constructor" {
                continue;
            }
            match var.text().replace(' ', "").as_str() {
                // `build = { modules = { ... } }`
                "build" => {
                    for field in field_nodes(&val) {
                        if field_name(&field).as_deref() == Some("modules")
                            && let Some(table) = field.field("value")
                            && table.kind() == "table_constructor"
                        {
                            out.extend(module_map(&table));
                        }
                    }
                }
                // `build.modules = { ... }`
                "build.modules" => out.extend(module_map(&val)),
                _ => {}
            }
        }
    }
    out
}

/// One `modules` table's literal entries. Anything computed contributes
/// nothing rather than a guess.
fn module_map(table: &SgNode) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for field in field_nodes(table) {
        let (Some(name), Some(value)) = (field_name(&field), field.field("value")) else {
            continue;
        };
        if let Some(path) = string_literal(&value) {
            out.push((name, path));
        }
    }
    out
}

/// The `(variable, value)` pairs one assignment writes, in source order.
///
/// A local copy of the extractor's pairing, kept private here because phase 0
/// reads a manifest rather than a source file and the two must be free to
/// disagree about what counts as a declaration.
fn pairs<'r>(assign: &SgNode<'r>) -> Vec<(SgNode<'r>, SgNode<'r>)> {
    let mut vars = Vec::new();
    let mut vals = Vec::new();
    for child in assign.children() {
        let target = match &*child.kind() {
            "variable_list" => &mut vars,
            "expression_list" => &mut vals,
            _ => continue,
        };
        target.extend(child.children().filter(|c| c.kind() != ","));
    }
    vars.into_iter().zip(vals).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nested_build_table_states_the_module_map() {
        let map = read_rockspec(
            "package = 'busted'\nbuild = {\n  type = 'builtin',\n  modules = {\n    \
             ['busted.core'] = 'busted/core.lua',\n    ['busted.init'] = 'busted/init.lua',\n  },\n\
             }\n",
        );
        assert_eq!(
            map,
            [
                ("busted.core".to_string(), "busted/core.lua".to_string()),
                ("busted.init".to_string(), "busted/init.lua".to_string()),
            ],
        );
    }

    #[test]
    fn the_flat_spelling_states_the_same_map() {
        let map = read_rockspec("build = {}\nbuild.modules = { foo = 'src/foo.lua' }\n");
        assert_eq!(map, [("foo".to_string(), "src/foo.lua".to_string())]);
    }

    #[test]
    fn only_build_modules_is_read_and_never_build_install_bin() {
        // The measured rockspec ends with
        // `install = { bin = { ['busted'] = 'bin/busted' } }`, a sibling of
        // `modules` under the same `build` table and written in the same
        // shape. Reading it would map the module name `busted` to a
        // command-line launcher script — silently resolving all 53 of the
        // corpus's genuinely ambiguous `require 'busted'` sites to the wrong
        // file, with nothing anywhere saying so.
        let map = read_rockspec(
            "build = {\n  type = 'builtin',\n  modules = { ['busted.core'] = 'busted/core.lua' },\n  \
             install = { bin = { ['busted'] = 'bin/busted' } },\n}\n",
        );
        assert_eq!(
            map,
            [("busted.core".to_string(), "busted/core.lua".to_string())]
        );
    }

    #[test]
    fn a_computed_entry_contributes_nothing_rather_than_a_guess() {
        // The corpus's own rockspec computes its `version` and its
        // `source.url` this way; a `modules` entry built the same way names a
        // file only the running program knows.
        let map = read_rockspec(
            "local n = 'busted'\nbuild = { modules = { [n] = n .. '.lua', ok = 'ok.lua' } }\n",
        );
        assert_eq!(map, [("ok".to_string(), "ok.lua".to_string())]);
    }

    #[test]
    fn dependencies_are_not_read_at_all() {
        // Reading them would be the cheapest way to inflate a rate, and the
        // manifest that declares them refutes the rock-name/module-name
        // identification six times out of nine.
        let map = read_rockspec(
            "dependencies = { 'penlight >= 1.15.0', 'say >= 1.4-1' }\n\
             build = { modules = { a = 'a.lua' } }\n",
        );
        assert_eq!(map, [("a".to_string(), "a.lua".to_string())]);
    }

    #[test]
    fn a_tree_with_no_manifest_still_has_a_layout() {
        let dir = tempfile::tempdir().expect("scratch");
        let cfg = layout(dir.path()).expect("a tree with no manifest still has a layout");
        assert!(cfg.modules.is_empty());
        assert!(cfg.rockspecs.is_empty());
    }

    #[test]
    fn both_manifest_directories_are_read_and_the_first_name_wins() {
        let dir = tempfile::tempdir().expect("scratch");
        std::fs::create_dir(dir.path().join(ROCKSPEC_DIR)).expect("rockspecs");
        std::fs::write(
            dir.path().join("a-1.rockspec"),
            "build = { modules = { m = 'from-root.lua' } }\n",
        )
        .expect("root manifest");
        std::fs::write(
            dir.path().join(ROCKSPEC_DIR).join("b-2.rockspec"),
            "build = { modules = { m = 'from-dir.lua', other = 'other.lua' } }\n",
        )
        .expect("released manifest");
        let cfg = layout(dir.path()).expect("layout");
        assert_eq!(cfg.rockspecs, ["a-1.rockspec", "rockspecs/b-2.rockspec"]);
        assert_eq!(cfg.declared_module("m"), Some("from-root.lua"));
        assert_eq!(cfg.declared_module("other"), Some("other.lua"));
        assert_eq!(cfg.declared_module("absent"), None);
    }

    #[test]
    fn the_digest_moves_when_the_map_does_and_not_otherwise() {
        let a = LuaProject {
            modules: [("m".to_string(), "m.lua".to_string())]
                .into_iter()
                .collect(),
            rockspecs: vec!["a.rockspec".to_string()],
        };
        let mut b = a.clone();
        assert_eq!(a.digest(), b.digest());
        b.modules.insert("n".to_string(), "n.lua".to_string());
        assert_ne!(a.digest(), b.digest());
        // A module name and a path cannot be confused for one another, and
        // neither can be confused for a manifest's own name: the sections are
        // separated by bytes no name or path carries.
        let c = LuaProject {
            modules: [("m".to_string(), "m.lua".to_string())]
                .into_iter()
                .collect(),
            rockspecs: vec!["a.rockspec".to_string(), "b.rockspec".to_string()],
        };
        assert_ne!(a.digest(), c.digest());
    }
}
