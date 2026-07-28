//! Phase 0 for Haskell: where a module name is looked up, and which packages
//! the project declares it does not contain.
//!
//! Haskell states neither in its source. A file writes `module
//! Data.Aeson.Key where` and nothing more; *where that module lives* is
//! `<root>/Data/Aeson/Key.hs` for some `hs-source-dirs` root of the component
//! being built, and only a `.cabal` file says which roots exist. The measured
//! corpus makes the point on its own — five manifests declare **fourteen**
//! `hs-source-dirs` entries over eight distinct roots, and
//! `Data.Aeson.Parser.Internal` sits under `attoparsec-aeson/src` while
//! `Data.Aeson` sits under `src`: one dotted-name-to-path rule, two roots, and
//! no `.hs` file naming either. So a `.cabal` is to Haskell what `go.mod` is
//! to Go and `Cargo.toml` is to Rust — a scan input the walk never hashes that
//! decides every identity beneath it, which is why it is fingerprinted by
//! [`crate::lang::Resolver::config_digest`].
//!
//! # What is read
//!
//! Every `*.cabal` file the walk reaches, and from each of them three things:
//!
//! - **`name:`** — the package this manifest declares. The set of them is what
//!   makes a `build-depends` entry a *dependency on something else*.
//! - **`hs-source-dirs:`** from every component — library, executable,
//!   test-suite, benchmark alike. A test tree is as much a home-module root as
//!   a library's, and a resolver that read only libraries would report a
//!   package's own tests as unresolvable.
//! - **`build-depends:`** from every component, `if` branches included. Both
//!   arms of a conditional are read for the same reason the extractor reads
//!   both arms of a `#ifdef`: choosing one means choosing a compiler.
//!
//! `cabal.project` is deliberately **not** read. It lists package
//! directories, which is a fact this walk establishes for itself by finding
//! their manifests, and aeson's names one — `benchmarks` — whose directory the
//! vendored snapshot excludes. A manifest naming a root that is not present is
//! exactly the shape that should be reported honestly rather than turned into
//! a phase-0 failure, and not reading the file is how that stays true.
//!
//! No file is executed, no compiler is consulted and no network call is made.
//! A `.cabal` field this reader cannot parse contributes nothing rather than a
//! guess.

use std::collections::BTreeSet;
use std::path::Path;

use crate::lang::LayoutError;

/// What the project's layout decides for every Haskell reference in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HsProject {
    /// Repo-relative source roots, in probe order: manifest path order, then
    /// declaration order within a manifest, deduplicated.
    ///
    /// Order is a committed fact and not an incidental one. A module name
    /// reachable under two roots resolves to the first one tried, and
    /// [`crate::track_haskell::resolve`] puts the *importing file's own* root
    /// at the head of this list before probing — which is GHC's rule, a home
    /// module is looked for in its own component's source dirs first.
    pub source_roots: Vec<String>,
    /// Every `hs-source-dirs` entry read, including repeats.
    ///
    /// Kept beside [`HsProject::source_roots`] because the two are different
    /// facts: aeson's five manifests declare fourteen entries over eight
    /// distinct roots, and seven components of `aeson-examples` all name
    /// `src/`. A drop in either is a manifest this reader stopped
    /// understanding.
    pub source_dir_entries: usize,
    /// Package names the manifests declare — one per `name:` field.
    pub packages: BTreeSet<String>,
    /// Package names the manifests depend on, runtime and test alike.
    ///
    /// Not separated by component, because the question a resolver asks is
    /// "does this repository say it links against code it does not contain?",
    /// and a test suite's dependency is as declared as a library's.
    pub dependencies: BTreeSet<String>,
    /// The manifests phase 0 read, repo-relative and sorted. Provenance for
    /// the digest, so that adding one re-roots the graph rather than silently
    /// widening it.
    pub manifests: Vec<String>,
    /// Every module name the walk found a file declaring, folded in by the
    /// driver through [`crate::lang::Resolver::learn_containers`].
    ///
    /// **Not phase 0, and deliberately not in [`HsProject::digest`].** It is
    /// learned from the store and from this event's own files as the scan
    /// runs, so fingerprinting it would wipe the store on every scan. Its one
    /// job is the anti-laundering guard: a name in here that no source root
    /// explains is this build's layout inference failing, and it must count
    /// against the rate rather than leave the denominator as `External`.
    pub declared_modules: BTreeSet<String>,
}

impl HsProject {
    /// Whether this repository declares a dependency on a package it does not
    /// itself contain.
    ///
    /// The gate on [`crate::Outcome::External`], and the only thing
    /// `build-depends` decides. `External` sits outside **both** terms of the
    /// resolution rate, so a resolver may only reach for it on evidence the
    /// repository itself states — and this is that evidence: every module in
    /// this tree is a home module of some root, so an import naming none of
    /// them must be supplied by a package the build declares and the tree does
    /// not hold. A repository whose manifests declare no such package has said
    /// no such thing, and its unknown modules stay
    /// [`crate::UnresolvedReason::UnknownPackage`] — inside the denominator,
    /// counting against the rate.
    pub fn declares_outside_dependency(&self) -> bool {
        self.dependencies.iter().any(|d| !self.packages.contains(d))
    }

    /// A stable fingerprint of everything phase 0 read.
    pub fn digest(&self) -> Vec<u8> {
        let mut out = String::new();
        for root in &self.source_roots {
            out.push_str(root);
            out.push('\n');
        }
        out.push('\u{1}');
        for pkg in &self.packages {
            out.push_str(pkg);
            out.push('\n');
        }
        out.push('\u{1}');
        for dep in &self.dependencies {
            out.push_str(dep);
            out.push('\n');
        }
        out.push('\u{1}');
        for manifest in &self.manifests {
            out.push_str(manifest);
            out.push('\n');
        }
        out.push('\u{1}');
        out.push_str(&self.source_dir_entries.to_string());
        out.into_bytes()
    }
}

/// Read the project's layout from the tree at `root`.
///
/// Never fails on a repository that declares nothing: a Haskell tree with no
/// `.cabal` is an ordinary shape — a script directory, a `runghc` sample — and
/// the honest consequence is that it has no source root at all, so nothing in
/// it is a home module and [`crate::track_haskell::resolve`] reports every
/// import as [`crate::UnresolvedReason::ProjectLayoutUnknown`] rather than
/// waving the whole denominator out as external.
pub fn layout(root: &Path) -> Result<HsProject, LayoutError> {
    let mut manifests: Vec<(String, String)> = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build() {
        let entry = match entry {
            Ok(entry) => entry,
            // A directory this walk may not descend into is a fact about the
            // tree, not a reason to refuse to measure it — except at the root,
            // where there is no tree to measure.
            Err(e) => {
                if let ignore::Error::WithPath { path, .. } = &e
                    && path == root
                {
                    return Err(LayoutError {
                        message: format!("reading {}: {e}", root.display()),
                    });
                }
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("cabal") || !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let Some(rel) = rel.to_str().map(|r| r.replace('\\', "/")) else {
            continue; // a path the store could not key this manifest under
        };
        if rel.split('/').any(|c| {
            <crate::track_haskell::lang::HsLang as crate::lang::Language>::skip_dirs().contains(&c)
        }) {
            continue; // build output: an unpacked dependency's manifest is not this project's
        }
        // A manifest that is not UTF-8 is not one this build can learn
        // anything from, and refusing the whole scan over one would turn a
        // manifest oddity into a missing measurement.
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        manifests.push((rel, source));
    }
    // Walk order is whatever the filesystem says; the probe order must not be.
    manifests.sort_by(|a, b| a.0.cmp(&b.0));

    let mut project = HsProject::default();
    for (rel, source) in &manifests {
        project.manifests.push(rel.clone());
        let dir = match rel.rfind('/') {
            Some(at) => &rel[..at],
            None => "",
        };
        let manifest = read_cabal(source);
        if let Some(name) = manifest.name {
            project.packages.insert(name);
        }
        for entry in manifest.source_dirs {
            project.source_dir_entries += 1;
            let joined = join_dir(dir, &entry);
            if !project.source_roots.contains(&joined) {
                project.source_roots.push(joined);
            }
        }
        project.dependencies.extend(manifest.dependencies);
    }
    Ok(project)
}

/// A source root, repo-relative: the manifest's directory joined with an
/// `hs-source-dirs` entry, with `.` and trailing slashes resolved.
///
/// `""` is the repository root, which is what `hs-source-dirs: .` in a
/// top-level manifest means.
fn join_dir(dir: &str, entry: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for segment in entry.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// The three fields one manifest states that this build reads.
#[derive(Debug, Default, PartialEq, Eq)]
struct Manifest {
    /// The package's own name, from the top-level `name:` field.
    name: Option<String>,
    /// Every `hs-source-dirs` entry, in declaration order and with repeats.
    source_dirs: Vec<String>,
    /// Every `build-depends` package name.
    dependencies: BTreeSet<String>,
}

/// Read one `.cabal` file.
///
/// Cabal's grammar is layout-sensitive: a field is `name: value` and its value
/// continues on every following line indented *more* than the field name. That
/// one rule is the whole parser, and it is what makes `if impl(ghc >=9.4)`
/// work — the `build-depends` inside the branch is a new field at a deeper
/// indent, not a continuation of the one above the branch.
fn read_cabal(source: &str) -> Manifest {
    let mut out = Manifest::default();
    let mut open: Option<(String, usize, String)> = None;
    let close = |open: &mut Option<(String, usize, String)>, out: &mut Manifest| {
        let Some((field, _, value)) = open.take() else {
            return;
        };
        match field.as_str() {
            "name" => {
                let name = value.trim().to_string();
                if !name.is_empty() && out.name.is_none() {
                    out.name = Some(name);
                }
            }
            "hs-source-dirs" => {
                for dir in value.split([',', ' ', '\t']) {
                    let dir = dir.trim();
                    if !dir.is_empty() {
                        out.source_dirs.push(dir.to_string());
                    }
                }
            }
            "build-depends" => {
                for part in value.split(',') {
                    if let Some(name) = package_name(part) {
                        out.dependencies.insert(name);
                    }
                }
            }
            _ => {}
        }
    };

    for raw in source.lines() {
        let trimmed = raw.trim_start();
        // A comment line is not a continuation and does not end a field:
        // aeson writes `-- Compat` between two `build-depends` blocks.
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }
        let indent = raw.len() - trimmed.len();
        if let Some((_, at, value)) = open.as_mut()
            && indent > *at
        {
            value.push(' ');
            value.push_str(trimmed);
            continue;
        }
        close(&mut open, &mut out);
        let Some(colon) = trimmed.find(':') else {
            continue; // a stanza header (`library`, `if impl(…)`) states no field
        };
        let field = trimmed[..colon].trim().to_ascii_lowercase();
        if field.is_empty() || !field.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            continue;
        }
        // `name:` names the package only at the top level. A stanza header
        // carries a component's name in its own line, so an indented `name:`
        // is some other field entirely and must not rename the package.
        if field == "name" && indent != 0 {
            continue;
        }
        open = Some((field, indent, trimmed[colon + 1..].to_string()));
    }
    close(&mut open, &mut out);
    out
}

/// The package name a `build-depends` entry opens with, or `None` when the
/// entry names none.
///
/// A dependency is `name` followed by an optional version range, and the range
/// may carry anything from `^>=0.1` to `>=0.4 && <0.6 || ^>=1.0.2`. Only the
/// leading name is read; a range this reader does not understand costs
/// nothing, because nothing downstream reads one.
fn package_name(entry: &str) -> Option<String> {
    let entry = entry.trim().trim_start_matches(',').trim();
    let name: String = entry
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    // A version range with no name in front of it — the tail of an entry this
    // reader already split, or a `mixins`-style clause — names no package.
    (!name.is_empty() && name.chars().any(|c| c.is_ascii_alphabetic())).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AESON: &str = "\
cabal-version:      2.2
name:               aeson
version:            2.3.1.0

library
  default-language: Haskell2010
  hs-source-dirs:   src
  exposed-modules:
    Data.Aeson

  -- GHC bundled libs
  build-depends:
    , base              >=4.12.0.0 && <5
    , bytestring        >=0.10.8.2 && <0.13

  -- Compat
  build-depends:
    , time-compat  >=1.9.6 && <1.10

  if !impl(ghc >=9.4)
    build-depends: generically >=0.1 && <0.2

  ghc-options:      -Wall

test-suite aeson-tests
  type:             exitcode-stdio-1.0
  hs-source-dirs:   tests
  main-is:          Tests.hs
  build-depends:
      aeson
    , Diff                  >=0.4    && <0.6  || ^>=1.0.2
";

    #[test]
    fn a_manifest_states_its_name_its_roots_and_its_dependencies() {
        let m = read_cabal(AESON);
        assert_eq!(m.name.as_deref(), Some("aeson"));
        // Two components, two entries — the test tree is a home-module root.
        assert_eq!(m.source_dirs, ["src", "tests"]);
        assert_eq!(
            m.dependencies
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "Diff",
                "aeson",
                "base",
                "bytestring",
                "generically",
                "time-compat"
            ],
        );
    }

    #[test]
    fn a_comment_between_two_blocks_of_one_field_ends_neither() {
        // aeson writes `-- Compat` between two `build-depends` blocks in one
        // stanza. A reader that treated the comment as a line at indent 2
        // would close the first block and then reopen — harmless — but one
        // that treated it as a *continuation* would swallow the field name of
        // the block after it.
        let m = read_cabal(AESON);
        assert!(m.dependencies.contains("bytestring"));
        assert!(m.dependencies.contains("time-compat"));
    }

    #[test]
    fn both_arms_of_a_conditional_contribute_their_dependencies() {
        // `if !impl(ghc >=9.4)` guards `generically`. Choosing an arm means
        // choosing a compiler; the union is the honest superset.
        assert!(read_cabal(AESON).dependencies.contains("generically"));
    }

    #[test]
    fn a_version_range_never_becomes_a_package() {
        let m = read_cabal(AESON);
        for name in &m.dependencies {
            assert!(
                name.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric()),
                "{name} is not a package name",
            );
        }
        assert_eq!(
            package_name("  , base >=4.12.0.0 && <5"),
            Some("base".into())
        );
        assert_eq!(package_name(" && <0.6  || ^>=1.0.2"), None);
        assert_eq!(package_name(""), None);
    }

    #[test]
    fn a_components_name_never_renames_the_package() {
        // `library twitter-generic` states a component name in its header;
        // only an indent-0 `name:` field names the package.
        let m = read_cabal(
            "name: aeson-examples\n\nlibrary twitter-generic\n  name: not-the-package\n  hs-source-dirs: src/\n",
        );
        assert_eq!(m.name.as_deref(), Some("aeson-examples"));
        assert_eq!(m.source_dirs, ["src/"]);
    }

    #[test]
    fn several_entries_on_one_line_are_several_roots() {
        let m = read_cabal("library\n  hs-source-dirs: src, ext lib\n");
        assert_eq!(m.source_dirs, ["src", "ext", "lib"]);
    }

    #[test]
    fn a_root_is_relative_to_its_manifests_directory() {
        assert_eq!(join_dir("text-iso8601", "src"), "text-iso8601/src");
        assert_eq!(join_dir("examples", "src/"), "examples/src");
        assert_eq!(join_dir("", "src"), "src");
        // `hs-source-dirs: .` in a top-level manifest is the repository root.
        assert_eq!(join_dir("", "."), "");
        assert_eq!(join_dir("pkg", "."), "pkg");
    }

    #[test]
    fn the_external_gate_is_a_dependency_this_tree_does_not_contain() {
        let mut cfg = HsProject {
            packages: ["aeson".to_string(), "text-iso8601".to_string()]
                .into_iter()
                .collect(),
            dependencies: ["aeson".to_string()].into_iter().collect(),
            ..HsProject::default()
        };
        // Every declared dependency is a package this repository holds, so
        // the repository has not said it links against anything outside it.
        assert!(!cfg.declares_outside_dependency());
        cfg.dependencies.insert("bytestring".to_string());
        assert!(cfg.declares_outside_dependency());
    }

    #[test]
    fn the_digest_moves_when_the_layout_does_and_not_otherwise() {
        let a = HsProject {
            source_roots: vec!["src".to_string()],
            source_dir_entries: 1,
            packages: ["aeson".to_string()].into_iter().collect(),
            dependencies: ["base".to_string()].into_iter().collect(),
            manifests: vec!["aeson.cabal".to_string()],
            declared_modules: BTreeSet::new(),
        };
        let mut b = a.clone();
        assert_eq!(a.digest(), b.digest());
        b.source_roots.push("tests".to_string());
        assert_ne!(a.digest(), b.digest());
        // A root, a package name and a dependency name cannot be confused for
        // one another: the sections are separated by a byte none of them
        // carries.
        let c = HsProject {
            source_roots: Vec::new(),
            source_dir_entries: 1,
            packages: ["src".to_string()].into_iter().collect(),
            dependencies: ["aeson".to_string(), "base".to_string()]
                .into_iter()
                .collect(),
            manifests: vec!["aeson.cabal".to_string()],
            declared_modules: BTreeSet::new(),
        };
        assert_ne!(a.digest(), c.digest());
        // Two manifests declaring one root is not one manifest declaring it:
        // a component that stopped being read must move the fingerprint.
        let mut d = a.clone();
        d.source_dir_entries = 2;
        assert_ne!(a.digest(), d.digest());
        // What the driver teaches as the scan runs is not part of the
        // project, and folding it in would wipe the store every scan.
        let mut learned = a.clone();
        learned.declared_modules.insert("Data.Aeson".to_string());
        assert_eq!(a.digest(), learned.digest());
    }

    #[test]
    fn a_tree_with_no_manifest_has_no_root_and_says_so() {
        let dir = tempfile::tempdir().expect("scratch");
        std::fs::create_dir(dir.path().join("src")).expect("src");
        let cfg = layout(dir.path()).expect("a tree with no manifest still has a layout");
        assert!(cfg.source_roots.is_empty());
        assert!(cfg.manifests.is_empty());
        // And it has declared nothing, so nothing in it may be called
        // external.
        assert!(!cfg.declares_outside_dependency());
    }

    #[test]
    fn manifests_are_read_in_path_order_whatever_the_walk_says() {
        let dir = tempfile::tempdir().expect("scratch");
        std::fs::write(
            dir.path().join("a.cabal"),
            "name: a\nlibrary\n  hs-source-dirs: src\n  build-depends: base\n",
        )
        .expect("a.cabal");
        std::fs::create_dir(dir.path().join("zsub")).expect("zsub");
        std::fs::write(
            dir.path().join("zsub/z.cabal"),
            "name: z\nlibrary\n  hs-source-dirs: src\n",
        )
        .expect("z.cabal");
        let cfg = layout(dir.path()).expect("layout");
        assert_eq!(cfg.manifests, ["a.cabal", "zsub/z.cabal"]);
        assert_eq!(cfg.source_roots, ["src", "zsub/src"]);
        assert_eq!(cfg.source_dir_entries, 2);
        assert_eq!(
            cfg.packages.iter().map(String::as_str).collect::<Vec<_>>(),
            ["a", "z"],
        );
        assert!(cfg.declares_outside_dependency());
    }
}
