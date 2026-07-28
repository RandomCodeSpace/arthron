//! Phase 0 for Dart: what `pubspec.yaml` says the package is called, and which
//! packages it declares.
//!
//! Dart states neither in its source, and the first of the two is the whole
//! reason this module exists. `import 'package:collection/collection.dart'`
//! resolves to `lib/collection.dart` **only because `pubspec.yaml` says
//! `name: collection`** — the segment after `package:` is a package name, not
//! a directory anywhere in the tree, and nothing in any `.dart` file connects
//! the two. A resolver handed the source without the manifest cannot tell that
//! URI from `package:test/test.dart`, which genuinely leaves the repository.
//! That is why this is fingerprinted by
//! [`crate::lang::Resolver::config_digest`]: a scan of the same tree under a
//! different `name:` describes a different graph.
//!
//! # What is read, and what is not
//!
//! Only `pubspec.yaml` **at the repository root**, which is where `dart pub`
//! expects one and where a single-package repository puts it. The manifest is
//! parsed with the grammar its own format has rather than scanned line by
//! line: a nested dependency block — `foo:` then `git:` then `url:` — is a
//! shape an indentation heuristic gets wrong and a parser does not.
//!
//! - **`name:`** is the package this repository *is*, and `lib/` beneath the
//!   manifest is what `package:<name>/…` addresses.
//! - **`dependencies:`, `dev_dependencies:` and `dependency_overrides:`** are
//!   read together, because the question a resolver asks is "does this
//!   repository say this name comes from outside?", and a test file importing
//!   a dev dependency is as declared as a library file importing a runtime
//!   one.
//! - **`pubspec.lock`, `.dart_tool/package_config.json`** are not read. The
//!   first names a resolved graph rather than a declaration; the second is
//!   build output that a checkout need not carry, and reading it would make
//!   the measured rate depend on whether `dart pub get` had been run.
//! - **Nested `pubspec.yaml` files.** A workspace of several packages declares
//!   names this build does not see, and their `package:` URIs miss with a
//!   reason rather than resolve by accident. The fix is a decision about which
//!   root a name maps against, not a loop.
//!
//! No file is executed and no network call is made, here or anywhere.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

use crate::lang::LayoutError;
use crate::sg::{Rules, SgNode, SourceTree};

/// The manifest file phase 0 reads, by the name `dart pub` gives it.
pub const MANIFEST: &str = "pubspec.yaml";

/// The directory `package:<name>/…` addresses, relative to the manifest.
pub const LIB: &str = "lib";

/// The pubspec keys whose child mapping names packages outside this
/// repository.
const DEPENDENCY_KEYS: &[&str] = &["dependencies", "dev_dependencies", "dependency_overrides"];

/// What the project's layout decides for every Dart URI in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DartProject {
    /// The package this repository *is*, from `pubspec.yaml`'s `name:`.
    ///
    /// `None` when no manifest was read or it declared no name — and then a
    /// `package:` URI cannot be told from the outside, which is
    /// [`crate::UnresolvedReason::ProjectLayoutUnknown`] rather than a guess.
    pub package: Option<String>,
    /// Package names the manifest declares as dependencies, runtime,
    /// development and override alike.
    pub dependencies: BTreeSet<String>,
    /// Whether a `pubspec.yaml` was found and parsed at the repository root.
    pub manifest: bool,
}

impl DartProject {
    /// The repo-relative path `package:<pkg>/<rest>` addresses, when `<pkg>`
    /// is this repository's own package.
    ///
    /// `None` for every other package name, including when the manifest
    /// declared none — the caller then has a different question to answer and
    /// must not be handed a path that looks like an answer.
    pub fn own_package_path(&self, pkg: &str, rest: &str) -> Option<String> {
        (self.package.as_deref() == Some(pkg) && !rest.is_empty()).then(|| format!("{LIB}/{rest}"))
    }

    /// Whether the manifest declares this package name as a dependency.
    pub fn declares(&self, pkg: &str) -> bool {
        self.dependencies.contains(pkg)
    }

    /// A stable fingerprint of everything phase 0 read.
    pub fn digest(&self) -> Vec<u8> {
        let mut out = String::new();
        if let Some(name) = &self.package {
            out.push_str(name);
        }
        out.push('\u{1}');
        for dep in &self.dependencies {
            out.push_str(dep);
            out.push('\n');
        }
        out.push('\u{1}');
        out.push(if self.manifest { '1' } else { '0' });
        out.into_bytes()
    }
}

/// Read the project's layout from the tree at `root`.
///
/// Never fails on a repository that declares nothing: a Dart tree with no
/// `pubspec.yaml` is an ordinary shape — a scratch directory, a subtree of a
/// workspace — and every relative import in it still resolves. Only
/// `package:` URIs lose their anchor, and they say so.
pub fn layout(root: &Path) -> Result<DartProject, LayoutError> {
    let path = root.join(MANIFEST);
    if !path.is_file() {
        return Ok(DartProject::default());
    }
    // A manifest that is not UTF-8 is not one this build can learn anything
    // from, and refusing the whole scan over it would turn a manifest oddity
    // into a missing measurement.
    let Ok(source) = std::fs::read_to_string(&path) else {
        return Ok(DartProject::default());
    };
    let (package, dependencies) = read_pubspec(&source);
    Ok(DartProject {
        package,
        dependencies,
        manifest: true,
    })
}

/// The one rule phase 0 needs: every `key: value` pair of the manifest,
/// block and flow spellings alike.
const PAIR_RULES: &str = "\
id: pair
language: yaml
rule:
  any:
    - kind: block_mapping_pair
    - kind: flow_pair
";

/// What one `pubspec.yaml` declares: its package name, and its dependency
/// names.
///
/// Depth is what makes this a parser rather than a scanner. A pair with no
/// enclosing pair is a pubspec key; a pair one level under a dependency key
/// is a package name; a `url:` two levels under one is neither, and an
/// indentation heuristic is exactly what mistakes it for a package.
fn read_pubspec(source: &str) -> (Option<String>, BTreeSet<String>) {
    static RULES: OnceLock<Rules> = OnceLock::new();
    let rules =
        RULES.get_or_init(|| Rules::compile(PAIR_RULES).expect("the pubspec rule compiles"));

    let mut package = None;
    let mut dependencies = BTreeSet::new();
    let tree = SourceTree::parse_yaml(source);
    for (_, pair) in tree.matches(rules) {
        let Some(key) = pair.field("key").as_ref().and_then(scalar) else {
            continue;
        };
        let enclosing = enclosing_keys(&pair);
        match enclosing.as_slice() {
            [] if key == "name" && package.is_none() => {
                package = pair
                    .field("value")
                    .as_ref()
                    .and_then(scalar)
                    .filter(|s| !s.is_empty());
            }
            [outer] if DEPENDENCY_KEYS.contains(&outer.as_str()) => {
                dependencies.insert(key);
            }
            _ => {}
        }
    }
    (package, dependencies)
}

/// The keys of the pairs enclosing this one, outermost first.
fn enclosing_keys(pair: &SgNode) -> Vec<String> {
    let mut out: Vec<String> = pair
        .ancestors()
        .filter(|a| matches!(&*a.kind(), "block_mapping_pair" | "flow_pair"))
        .map(|a| a.field("key").as_ref().and_then(scalar).unwrap_or_default())
        .collect();
    out.reverse();
    out
}

/// A YAML scalar's text, quotes removed. `None` when the node is not one
/// plain or quoted scalar — a mapping, a sequence, an anchor, a block scalar.
fn scalar(node: &SgNode) -> Option<String> {
    match &*node.kind() {
        "flow_node" | "block_node" => node.children().find_map(|c| scalar(&c)),
        "plain_scalar" => Some(node.text().trim().to_string()),
        "single_quote_scalar" => Some(unquote(&node.text(), '\'')),
        "double_quote_scalar" => Some(unquote(&node.text(), '"')),
        _ => None,
    }
}

/// A quoted scalar's contents. Escapes are left as written: a package name
/// containing one is not a name `dart pub` accepts, and inventing an
/// unescaping here would only make a malformed manifest look well formed.
fn unquote(text: &str, quote: char) -> String {
    text.trim()
        .trim_start_matches(quote)
        .trim_end_matches(quote)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLLECTION: &str = "\
name: collection
version: 1.19.1
environment:
  sdk: ^3.4.0

dev_dependencies:
  dart_flutter_team_lints: ^3.0.0
  test: ^1.16.0
";

    #[test]
    fn the_package_name_and_its_dependencies_are_read() {
        let (package, deps) = read_pubspec(COLLECTION);
        assert_eq!(package.as_deref(), Some("collection"));
        assert_eq!(
            deps.iter().map(String::as_str).collect::<Vec<_>>(),
            ["dart_flutter_team_lints", "test"],
        );
    }

    #[test]
    fn a_nested_dependency_block_contributes_its_package_and_not_its_fields() {
        // The shape a line scanner gets wrong: `url` and `ref` are three
        // levels down and are not package names.
        let (_, deps) = read_pubspec(
            "name: app\ndependencies:\n  a: ^1.0.0\n  b:\n    git:\n      url: https://x.invalid/b\n      ref: main\n",
        );
        assert_eq!(
            deps.iter().map(String::as_str).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn quoted_and_flow_spellings_are_the_same_declaration() {
        let (package, deps) =
            read_pubspec("name: \"app\"\ndev_dependencies: {test: any, 'lints': ^1.0.0}\n");
        assert_eq!(package.as_deref(), Some("app"));
        assert_eq!(
            deps.iter().map(String::as_str).collect::<Vec<_>>(),
            ["lints", "test"]
        );
    }

    #[test]
    fn overrides_are_declarations_too_and_a_sequence_is_not() {
        let (_, deps) = read_pubspec(
            "name: app\ndependency_overrides:\n  a: any\ntopics:\n - collections\n - x\n",
        );
        assert_eq!(deps.iter().map(String::as_str).collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn a_package_urs_own_path_is_lib_and_only_for_its_own_name() {
        let cfg = DartProject {
            package: Some("collection".to_string()),
            dependencies: ["test".to_string()].into_iter().collect(),
            manifest: true,
        };
        assert_eq!(
            cfg.own_package_path("collection", "src/algorithms.dart"),
            Some("lib/src/algorithms.dart".to_string()),
        );
        // Another package's URI is not this repository's path, whether or not
        // the manifest declares it.
        assert_eq!(cfg.own_package_path("test", "test.dart"), None);
        assert_eq!(cfg.own_package_path("nowhere", "x.dart"), None);
        assert!(cfg.declares("test"));
        assert!(!cfg.declares("collection"));
    }

    #[test]
    fn a_tree_with_no_manifest_has_no_package_and_says_so() {
        let dir = tempfile::tempdir().expect("scratch");
        let cfg = layout(dir.path()).expect("a tree with no manifest still has a layout");
        assert!(!cfg.manifest);
        assert_eq!(cfg.package, None);
        assert!(cfg.dependencies.is_empty());
    }

    #[test]
    fn the_manifest_at_the_root_is_the_one_that_is_read() {
        let dir = tempfile::tempdir().expect("scratch");
        std::fs::write(dir.path().join(MANIFEST), COLLECTION).expect("pubspec");
        std::fs::create_dir_all(dir.path().join("pkgs/other")).expect("nested");
        std::fs::write(
            dir.path().join("pkgs/other").join(MANIFEST),
            "name: other\ndependencies:\n  http: any\n",
        )
        .expect("nested pubspec");
        let cfg = layout(dir.path()).expect("layout");
        assert_eq!(cfg.package.as_deref(), Some("collection"));
        assert!(!cfg.declares("http"), "a nested manifest was read");
    }

    #[test]
    fn the_digest_moves_when_the_layout_does_and_not_otherwise() {
        let a = DartProject {
            package: Some("collection".to_string()),
            dependencies: BTreeSet::new(),
            manifest: true,
        };
        let mut b = a.clone();
        assert_eq!(a.digest(), b.digest());
        b.dependencies.insert("test".to_string());
        assert_ne!(a.digest(), b.digest());
        // A package name and a dependency name cannot be confused for one
        // another: the sections are separated by a byte no name carries.
        let c = DartProject {
            package: None,
            dependencies: ["collection".to_string()].into_iter().collect(),
            manifest: true,
        };
        assert_ne!(a.digest(), c.digest());
        // Whether a manifest was read at all is part of the fingerprint: a
        // tree that gains one re-roots every `package:` URI in it.
        let d = DartProject {
            manifest: false,
            ..a.clone()
        };
        assert_ne!(a.digest(), d.digest());
    }
}
