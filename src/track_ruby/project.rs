//! Phase 0 for Ruby: where the load path starts, and which gems the project
//! declares.
//!
//! Ruby states neither in its source. A `require 'rack/utils'` is resolved by
//! `$LOAD_PATH`, and what is on `$LOAD_PATH` for a gem is what its gemspec
//! calls `require_paths` — defaulting to `lib`. That one line is to Ruby what
//! the `module` directive is to Go: it roots every candidate the resolver
//! builds, which is why it is fingerprinted by
//! [`crate::lang::Resolver::config_digest`] and why a scan of the same tree
//! under a different one is a different graph.
//!
//! # What is read, and what is not
//!
//! Only `*.gemspec` files **at the repository root**, which is where `gem
//! build` expects one and where every single-gem repository puts it. A
//! `Gemfile` is deliberately not read: it names the gems an *application*
//! locks, not the ones this source declares, and `Gemfile.lock` names a
//! resolved graph rather than a declaration. A repository of several gems —
//! one gemspec per subdirectory — is a shape no measurement here covers yet,
//! and the honest consequence is that its nested `lib/` directories are not
//! load roots, so requires into them miss with a reason rather than resolve
//! by accident.
//!
//! A gemspec is Ruby source, and it is parsed as Ruby rather than pattern
//! matched: `s.add_development_dependency 'minitest', "~> 5.0"` is a call
//! whose first argument is a literal, and reading it as one is both simpler
//! and less wrong than a regular expression over the same bytes.
//!
//! No file is executed and no network call is made, here or anywhere: a
//! gemspec that computes its dependency list contributes the literals it
//! states and nothing else.

use std::collections::BTreeSet;
use std::path::Path;

use crate::lang::LayoutError;
use crate::sg::{Rules, SgNode, SourceTree};
use crate::track_ruby::extract::{arg_nodes, string_literal};

/// The load path RubyGems gives a gem that declares none.
const DEFAULT_REQUIRE_PATHS: &[&str] = &["lib"];

/// What the project's layout decides for every Ruby reference in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RubyProject {
    /// Repo-relative load roots, in probe order. `""` is the repository root.
    ///
    /// Order is the probe order, so it is a committed fact and not an
    /// incidental one: a name reachable under two roots resolves to the first,
    /// which is what Ruby itself does with `$LOAD_PATH`.
    pub load_roots: Vec<String>,
    /// Gem names the project declares as dependencies, runtime and
    /// development alike.
    ///
    /// The two are not separated because the question a resolver asks is
    /// "does this repository say this name comes from outside?", and a test
    /// file requiring a development dependency is as declared as a library
    /// file requiring a runtime one.
    pub dependencies: BTreeSet<String>,
    /// The gemspecs phase 0 read, repo-relative and sorted. Provenance for
    /// the digest, so that adding a manifest re-roots the graph rather than
    /// silently widening it.
    pub gemspecs: Vec<String>,
}

impl RubyProject {
    /// The gem a specifier names, when the project declares it.
    ///
    /// Longest declared prefix first, with `/` written as `-`, because that
    /// is how a gem's name and its require path relate:
    /// `minitest/global_expectations/autorun` is shipped by
    /// `minitest-global_expectations`, not by `minitest`, and both are
    /// declared. A first-segment answer would classify the reference
    /// correctly and still name the wrong package on the node the reference
    /// points at.
    ///
    /// Which prefixes are tried is bounded by what the project declares, so
    /// this widens no bucket: a specifier whose every prefix is undeclared is
    /// `UnknownPackage` exactly as before.
    pub fn declared_gem(&self, spec: &str) -> Option<&str> {
        let segments: Vec<&str> = spec.split('/').collect();
        for take in (1..=segments.len()).rev() {
            let name = segments[..take].join("-");
            if let Some(gem) = self.dependencies.get(name.as_str()) {
                return Some(gem);
            }
        }
        None
    }

    /// A stable fingerprint of everything phase 0 read.
    pub fn digest(&self) -> Vec<u8> {
        let mut out = String::new();
        for root in &self.load_roots {
            out.push_str(root);
            out.push('\n');
        }
        out.push('\u{1}');
        for gem in &self.dependencies {
            out.push_str(gem);
            out.push('\n');
        }
        out.push('\u{1}');
        for spec in &self.gemspecs {
            out.push_str(spec);
            out.push('\n');
        }
        out.into_bytes()
    }
}

/// Read the project's layout from the tree at `root`.
///
/// Never fails on a repository that declares nothing: a Ruby tree with no
/// gemspec is an ordinary shape — a script directory, an application — and
/// the default is `lib` when it exists, falling back to the repository root
/// only when nothing else is a load root at all.
pub fn layout(root: &Path) -> Result<RubyProject, LayoutError> {
    let mut gemspecs: Vec<String> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    let mut dependencies: BTreeSet<String> = BTreeSet::new();

    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            return Err(LayoutError {
                message: format!("reading {}: {e}", root.display()),
            });
        }
    };
    let mut manifests: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("gemspec")
            && path.is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            manifests.push(name.to_string());
        }
    }
    // Directory order is whatever the filesystem says; the probe order must
    // not be.
    manifests.sort();

    for name in &manifests {
        // A gemspec that cannot be read as UTF-8 is not a gemspec this build
        // can learn anything from, and refusing the whole scan over one would
        // turn a manifest oddity into a missing measurement.
        let Ok(source) = std::fs::read_to_string(root.join(name)) else {
            continue;
        };
        gemspecs.push(name.clone());
        let (paths, gems) = read_gemspec(&source);
        for path in paths {
            push_root(&mut roots, path);
        }
        dependencies.extend(gems);
    }

    if manifests.is_empty() {
        for default in DEFAULT_REQUIRE_PATHS {
            if root.join(default).is_dir() {
                push_root(&mut roots, (*default).to_string());
            }
        }
    }
    // Only when nothing else is a load root: a flat script directory has one
    // anchor and it is itself. Behind `lib` it would be a second root Ruby
    // does not have — `$LOAD_PATH` has not carried the working directory
    // since 1.9 — and every repo-relative path in the tree would become
    // probeable, resolving requires the interpreter would refuse.
    if roots.is_empty() {
        push_root(&mut roots, String::new());
    }

    Ok(RubyProject {
        load_roots: roots,
        dependencies,
        gemspecs,
    })
}

/// Append a load root, keeping the first occurrence's position.
fn push_root(roots: &mut Vec<String>, root: String) {
    let normalized = root.trim_matches('/').to_string();
    if !roots.contains(&normalized) {
        roots.push(normalized);
    }
}

/// What one gemspec declares: its require paths, and its dependency names.
///
/// A gemspec that names no `require_paths` gets RubyGems' own default, which
/// is why the answer is never empty.
fn read_gemspec(source: &str) -> (Vec<String>, BTreeSet<String>) {
    static RULES: std::sync::OnceLock<Rules> = std::sync::OnceLock::new();
    let rules = RULES.get_or_init(|| {
        Rules::compile(
            "id: call\nlanguage: ruby\nrule:\n  kind: call\n\
             ---\nid: assign\nlanguage: ruby\nrule:\n  kind: assignment\n",
        )
        .expect("the gemspec rules compile")
    });

    let mut paths: Vec<String> = Vec::new();
    let mut gems: BTreeSet<String> = BTreeSet::new();
    let tree = SourceTree::parse_ruby(source);
    for (rule, node) in tree.matches(rules) {
        match rule {
            "call" => {
                let Some(method) = node.field("method") else {
                    continue;
                };
                if !matches!(
                    &*method.text(),
                    "add_dependency" | "add_runtime_dependency" | "add_development_dependency"
                ) {
                    continue;
                }
                if let Some(name) = arg_nodes(&node).first().and_then(string_literal) {
                    gems.insert(name);
                }
            }
            "assign" => {
                let Some(left) = node.field("left") else {
                    continue;
                };
                let target = left.text();
                let target = target.rsplit('.').next().unwrap_or(&target);
                if !matches!(target, "require_paths" | "require_path") {
                    continue;
                }
                let Some(right) = node.field("right") else {
                    continue;
                };
                paths.extend(literal_strings(&right));
            }
            _ => {}
        }
    }
    if paths.is_empty() {
        paths = DEFAULT_REQUIRE_PATHS
            .iter()
            .map(|p| (*p).to_string())
            .collect();
    }
    (paths, gems)
}

/// Every plain string literal a node states: itself, or the members of an
/// array or `%w[]` list. Anything computed contributes nothing.
fn literal_strings(node: &SgNode) -> Vec<String> {
    if let Some(one) = string_literal(node) {
        return vec![one];
    }
    let mut out = Vec::new();
    for child in node.children() {
        match &*child.kind() {
            "string" | "chained_string" => out.extend(string_literal(&child)),
            "bare_string" | "string_array" => {
                for inner in child.children() {
                    out.extend(string_literal(&inner));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gemspec_without_require_paths_gets_rubygems_default() {
        let (paths, gems) = read_gemspec(
            "Gem::Specification.new do |s|\n  s.name = \"rack\"\n  \
             s.add_development_dependency 'minitest', \"~> 5.0\"\n  \
             s.add_dependency 'rack-session'\nend\n",
        );
        assert_eq!(paths, ["lib"]);
        assert_eq!(
            gems.iter().map(String::as_str).collect::<Vec<_>>(),
            ["minitest", "rack-session"],
        );
    }

    #[test]
    fn a_declared_require_path_replaces_the_default() {
        let (paths, _) = read_gemspec(
            "Gem::Specification.new do |s|\n  s.require_paths = [\"src\", \"ext\"]\nend\n",
        );
        assert_eq!(paths, ["src", "ext"]);
    }

    #[test]
    fn a_singular_require_path_is_read_too() {
        let (paths, _) = read_gemspec("Gem::Specification.new { |s| s.require_path = 'code' }\n");
        assert_eq!(paths, ["code"]);
    }

    #[test]
    fn a_computed_dependency_contributes_nothing_rather_than_a_guess() {
        let (_, gems) = read_gemspec(
            "Gem::Specification.new do |s|\n  DEPS.each { |d| s.add_dependency d }\nend\n",
        );
        assert!(gems.is_empty(), "{gems:?}");
    }

    #[test]
    fn the_longest_declared_prefix_of_a_specifier_names_the_gem() {
        let cfg = RubyProject {
            load_roots: vec!["lib".to_string()],
            dependencies: [
                "minitest".to_string(),
                "minitest-global_expectations".to_string(),
            ]
            .into_iter()
            .collect(),
            gemspecs: vec!["x.gemspec".to_string()],
        };
        // Both are declared and both are prefixes; the file really is shipped
        // by the longer one.
        assert_eq!(
            cfg.declared_gem("minitest/global_expectations/autorun"),
            Some("minitest-global_expectations"),
        );
        // The first segment still answers when it is the only declared
        // prefix.
        assert_eq!(cfg.declared_gem("minitest/autorun"), Some("minitest"));
        // And an undeclared name is still undeclared: no prefix of it was
        // declared either, so nothing moved out of `UnknownPackage`.
        assert_eq!(cfg.declared_gem("time"), None);
        assert_eq!(cfg.declared_gem("global_expectations/minitest"), None);
    }

    #[test]
    fn the_digest_moves_when_the_layout_does_and_not_otherwise() {
        let a = RubyProject {
            load_roots: vec!["lib".to_string()],
            dependencies: BTreeSet::new(),
            gemspecs: Vec::new(),
        };
        let mut b = a.clone();
        assert_eq!(a.digest(), b.digest());
        b.load_roots.push("ext".to_string());
        assert_ne!(a.digest(), b.digest());
        // A gem name and a load root cannot be confused for one another: the
        // sections are separated by a byte no path or gem name carries.
        let c = RubyProject {
            load_roots: Vec::new(),
            dependencies: ["lib".to_string()].into_iter().collect(),
            gemspecs: Vec::new(),
        };
        assert_ne!(a.digest(), c.digest());
    }

    #[test]
    fn a_tree_with_no_gemspec_gets_lib_and_not_the_root_behind_it() {
        // The root behind `lib` would make every repo-relative path in the
        // tree probeable, so `require 'foo/bar'` would resolve to
        // `foo/bar.rb` anywhere — a file Ruby itself would not find, since
        // `$LOAD_PATH` has not held the working directory since 1.9.
        let dir = tempfile::tempdir().expect("scratch");
        std::fs::create_dir(dir.path().join("lib")).expect("lib");
        let cfg = layout(dir.path()).expect("a tree with no manifest still has a layout");
        assert_eq!(cfg.load_roots, ["lib"]);
        assert!(cfg.gemspecs.is_empty());
    }

    #[test]
    fn a_flat_script_directory_is_its_own_load_root() {
        // Nothing else is one, and a tree with no load root at all would
        // resolve no `require` in it. This is the floor, not a second root.
        let dir = tempfile::tempdir().expect("scratch");
        let cfg = layout(dir.path()).expect("layout");
        assert_eq!(cfg.load_roots, [""]);
    }

    #[test]
    fn a_gemspec_decides_the_load_path_and_the_root_is_not_added_behind_it() {
        let dir = tempfile::tempdir().expect("scratch");
        std::fs::create_dir(dir.path().join("lib")).expect("lib");
        std::fs::write(
            dir.path().join("rack.gemspec"),
            "Gem::Specification.new do |s|\n  s.add_dependency 'nio4r'\nend\n",
        )
        .expect("gemspec");
        let cfg = layout(dir.path()).expect("layout");
        assert_eq!(cfg.load_roots, ["lib"]);
        assert_eq!(cfg.gemspecs, ["rack.gemspec"]);
        assert_eq!(cfg.declared_gem("nio4r"), Some("nio4r"));
    }
}
