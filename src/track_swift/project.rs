//! Phase 0 for Swift: which modules this package builds, and which files each
//! one is made of.
//!
//! Swift states neither in its source. A module is a whole SwiftPM **target**,
//! and a target's name and its directory are written in `Package.swift` —
//! nowhere else. That one manifest is to Swift what the `module` directive is
//! to Go: it decides the identity of every node below it, which is why it is
//! fingerprinted by [`crate::lang::Resolver::config_digest`] and why a scan of
//! the same tree under a different one is a different graph.
//!
//! # Which manifest, when a package ships four
//!
//! SwiftPM picks among `Package.swift` and `Package@swift-<version>.swift` by
//! *toolchain* version: a 6.0 toolchain reads `Package@swift-6.0.swift`, and a
//! toolchain newer than every suffix reads the plain `Package.swift`. arthron
//! runs no toolchain, so it reads the manifest with the **highest declared
//! `swift-tools-version`**, and `Package.swift` wins a tie — that is the
//! package as the newest toolchain it supports sees it. The measured corpus is
//! exactly this shape: four manifests, tools-versions 6.3, 6.2, 6.1 and 6.0,
//! and the plain `Package.swift` at 6.3 is the one read.
//!
//! *Rejected:* unioning all four, which would describe a package no toolchain
//! ever builds. *Also rejected:* reading only `Package.swift`, which is right
//! here by luck and wrong for any package whose newest manifest carries a
//! suffix. Every manifest found is still named in the digest, so adding one
//! re-roots the graph rather than silently changing nothing.
//!
//! # What is read, and what is not
//!
//! Only the `targets:` argument of the `Package(…)` call, and only the
//! `name:`, `path:`, `exclude:` and `sources:` of each target factory inside
//! it. Reading `.target(…)` calls anywhere else in the file would mint a
//! phantom module out of `dependencies: [.target(name: "Foo")]`, which names a
//! dependency rather than declaring a target.
//!
//! A manifest is Swift source, and it is parsed as Swift rather than pattern
//! matched. No file is executed and no network call is made: a manifest that
//! computes its target list contributes the literals it states and nothing
//! else — and if that leaves no target at all, the layout is *unknown* rather
//! than empty, which is what stops an unread manifest from laundering every
//! import in the package into `External`.

use std::path::Path;

use crate::lang::LayoutError;
use crate::sg::{Rules, SgNode, SourceTree};
use crate::track_swift::extract::string_literal;

/// The manifest SwiftPM reads when no versioned one matches.
const BASE_MANIFEST: &str = "Package.swift";

/// The prefix a version-suffixed manifest carries.
const VERSIONED_PREFIX: &str = "Package@swift-";

/// The default source directory of a regular target: `Sources/<name>`.
const DEFAULT_SOURCE_DIR: &str = "Sources";

/// The default source directory of a test target: `Tests/<name>`.
const DEFAULT_TEST_DIR: &str = "Tests";

/// What kind of target a module is built from.
///
/// Kept because the kind decides the default directory when a target states
/// no `path:`, and because two of them build no Swift at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetKind {
    /// `.target` — a library module.
    Regular,
    /// `.testTarget` — a test module. Defaults under `Tests/` rather than
    /// `Sources/`.
    Test,
    /// `.executableTarget`.
    Executable,
    /// `.macro` — a compiler-plugin module.
    Macro,
    /// `.plugin` — a build-tool or command plugin.
    Plugin,
    /// `.systemLibrary` — a module map over a system C library. Builds no
    /// Swift source of its own.
    System,
    /// `.binaryTarget` — a pre-built artifact. Builds no Swift source of its
    /// own.
    Binary,
}

impl TargetKind {
    /// The factory function that declares this kind, without its leading dot.
    pub fn factory(self) -> &'static str {
        match self {
            TargetKind::Regular => "target",
            TargetKind::Test => "testTarget",
            TargetKind::Executable => "executableTarget",
            TargetKind::Macro => "macro",
            TargetKind::Plugin => "plugin",
            TargetKind::System => "systemLibrary",
            TargetKind::Binary => "binaryTarget",
        }
    }

    /// Every kind, in the order a manifest reader tries them.
    pub const ALL: &'static [TargetKind] = &[
        TargetKind::Regular,
        TargetKind::Test,
        TargetKind::Executable,
        TargetKind::Macro,
        TargetKind::Plugin,
        TargetKind::System,
        TargetKind::Binary,
    ];

    /// The kind a factory name declares, if any.
    pub fn from_factory(name: &str) -> Option<TargetKind> {
        TargetKind::ALL
            .iter()
            .copied()
            .find(|k| k.factory() == name)
    }
}

/// One SwiftPM target: one module, and the files it is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The target's name, which is the module name source code imports.
    pub name: String,
    /// What kind of target it is.
    pub kind: TargetKind,
    /// Repo-relative, `/`-separated directory holding its sources. `""` is
    /// the repository root.
    pub dir: String,
    /// Paths the target excludes, relative to [`Target::dir`], sorted.
    pub excludes: Vec<String>,
    /// The explicit source list, relative to [`Target::dir`], sorted. Empty
    /// means "everything under the directory", which is SwiftPM's default.
    pub sources: Vec<String>,
}

impl Target {
    /// Whether a repo-relative path is one of this target's sources.
    fn owns(&self, rel: &str) -> bool {
        let Some(sub) = under(&self.dir, rel) else {
            return false;
        };
        if self
            .excludes
            .iter()
            .any(|e| sub == e || under(e, sub).is_some())
        {
            return false;
        }
        self.sources.is_empty()
            || self
                .sources
                .iter()
                .any(|s| sub == s || under(s, sub).is_some())
    }
}

/// The part of `rel` below `dir`, or `None` when it is not below it.
///
/// `""` is the repository root and is below nothing, so it contains
/// everything.
fn under<'a>(dir: &str, rel: &'a str) -> Option<&'a str> {
    if dir.is_empty() {
        return Some(rel);
    }
    rel.strip_prefix(dir)?.strip_prefix('/')
}

/// The package a Swift scan resolves against.
///
/// This is `SwiftLang`'s configuration: the driver moves it between phases and
/// never inspects it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SwiftPackage {
    /// The declared package name, or `""` when no manifest stated one.
    pub name: String,
    /// Every `Package*.swift` at the repository root, sorted. Provenance for
    /// the digest: adding a manifest can change which one is read, so it must
    /// re-root the graph rather than silently change nothing.
    pub manifests: Vec<String>,
    /// The manifest that was read, or `""` when there was none.
    pub manifest: String,
    /// The `swift-tools-version` of the manifest that was read.
    pub tools_version: String,
    /// Every declared target, sorted by name.
    pub targets: Vec<Target>,
}

/// The reserved prefix a module identity carries when no target claims the
/// file, and nothing else may.
///
/// A SwiftPM target name is a Swift module name, and a Swift identifier may
/// not begin with `$` — the compiler reserves that spelling — so no target's
/// identity can ever collide with one of these, whatever a repository names
/// its directories.
pub const ORPHAN: char = '$';

impl SwiftPackage {
    /// Whether the module namespace is known at all.
    ///
    /// False when no manifest was found, and false when one was found but
    /// stated no target this reader could read. Both mean the same thing: this
    /// build cannot say which modules the package builds, so it may not say
    /// that a name is outside it either. That is the whole guard against an
    /// unread manifest laundering every import in the package into `External`,
    /// where it would sit outside both terms of the resolution rate.
    pub fn known(&self) -> bool {
        !self.targets.is_empty()
    }

    /// Whether a module name is a target this package builds.
    pub fn is_target(&self, name: &str) -> bool {
        self.targets.iter().any(|t| t.name == name)
    }

    /// The target a repo-relative file belongs to.
    ///
    /// The most specific directory wins, so a target nested inside another's
    /// directory takes its own files. SwiftPM forbids overlapping target
    /// directories, so at most one answer is ever right; picking the longest
    /// match makes the answer deterministic even when a manifest breaks that
    /// rule.
    pub fn target_of(&self, rel: &str) -> Option<&Target> {
        self.targets
            .iter()
            .filter(|t| t.owns(rel))
            .max_by_key(|t| t.dir.len())
    }

    /// The module identity of a repo-relative file.
    ///
    /// A file no target claims — a manifest at the repository root, a script
    /// beside it — is its own module, named by its path under [`ORPHAN`].
    /// SwiftPM really does compile each manifest as a module of its own, and
    /// giving them one shared identity would merge four separate `let package`
    /// declarations into one node that no toolchain builds.
    pub fn module_fqn(&self, rel: &str) -> String {
        match self.target_of(rel) {
            Some(t) => t.name.clone(),
            None => format!("{ORPHAN}{}", rel.strip_suffix(".swift").unwrap_or(rel)),
        }
    }

    /// A stable fingerprint of everything phase 0 read.
    pub fn digest(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(&self.name);
        out.push('\u{1}');
        out.push_str(&self.manifest);
        out.push('\u{1}');
        out.push_str(&self.tools_version);
        out.push('\u{1}');
        for m in &self.manifests {
            out.push_str(m);
            out.push('\n');
        }
        out.push('\u{1}');
        for t in &self.targets {
            out.push_str(t.kind.factory());
            out.push('\u{2}');
            out.push_str(&t.name);
            out.push('\u{2}');
            out.push_str(&t.dir);
            out.push('\u{2}');
            out.push_str(&t.excludes.join(","));
            out.push('\u{2}');
            out.push_str(&t.sources.join(","));
            out.push('\n');
        }
        out.into_bytes()
    }
}

/// Read the package's layout from the tree at `root`.
///
/// Never fails on a repository that declares nothing: a Swift tree with no
/// manifest is an ordinary shape — an Xcode project, a script directory — and
/// the honest consequence is that [`SwiftPackage::known`] answers `false` and
/// every import in it is [`crate::UnresolvedReason::ProjectLayoutUnknown`],
/// which says the failure is arthron's own inference rather than a name that
/// is absent.
pub fn layout(root: &Path) -> Result<SwiftPackage, LayoutError> {
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
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == BASE_MANIFEST || (name.starts_with(VERSIONED_PREFIX) && name.ends_with(".swift"))
        {
            manifests.push(name.to_string());
        }
    }
    // Directory order is whatever the filesystem says; which manifest is read
    // must not be.
    manifests.sort();

    let mut chosen: Option<(Vec<u32>, String, String)> = None;
    for name in &manifests {
        let Ok(source) = std::fs::read_to_string(root.join(name)) else {
            continue;
        };
        let stated = tools_version(&source);
        let rank = version_key(&stated);
        // The plain `Package.swift` wins a tie: it is the manifest a toolchain
        // newer than every suffix reads.
        let better = match &chosen {
            None => true,
            Some((best, best_name, _)) => {
                rank > *best
                    || (rank == *best && name == BASE_MANIFEST && best_name != BASE_MANIFEST)
            }
        };
        if better {
            chosen = Some((rank, name.clone(), stated));
        }
    }

    let mut package = SwiftPackage {
        manifests,
        ..SwiftPackage::default()
    };
    if let Some((_, name, version)) = chosen {
        let source = std::fs::read_to_string(root.join(&name)).unwrap_or_default();
        let (pkg_name, targets) = read_manifest(&source);
        package.manifest = name;
        package.tools_version = version;
        package.name = pkg_name;
        package.targets = targets;
    }
    Ok(package)
}

/// The `swift-tools-version` a manifest states, as written.
///
/// The pragma is a comment on the first line — `// swift-tools-version: 6.3`,
/// with or without the space — and it is the one thing in a manifest that is
/// not Swift, so it is read as text rather than parsed.
fn tools_version(source: &str) -> String {
    let first = source.lines().next().unwrap_or_default();
    let Some((_, rest)) = first.split_once("swift-tools-version") else {
        return String::new();
    };
    rest.trim_start()
        .trim_start_matches(':')
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect()
}

/// A tools version as comparable components. An unstated version sorts below
/// every stated one.
fn version_key(version: &str) -> Vec<u32> {
    let mut parts: Vec<u32> = version
        .split('.')
        .map(|p| p.parse::<u32>().unwrap_or(0))
        .collect();
    parts.resize(3, 0);
    parts
}

/// The compiled manifest rules: every call, which is all a manifest is.
fn manifest_rules() -> &'static Rules {
    static RULES: std::sync::OnceLock<Rules> = std::sync::OnceLock::new();
    RULES.get_or_init(|| {
        Rules::compile("id: call\nlanguage: swift\nrule:\n  kind: call_expression\n")
            .expect("the manifest rule compiles")
    })
}

/// What one manifest declares: the package's name, and its targets.
fn read_manifest(source: &str) -> (String, Vec<Target>) {
    let tree = SourceTree::parse_swift(source);
    let mut name = String::new();
    let mut targets: Vec<Target> = Vec::new();
    for (_, node) in tree.matches(manifest_rules()) {
        if callee(&node).as_deref() != Some("Package") {
            continue;
        }
        let args = arguments(&node);
        if let Some(literal) = argument(&args, "name").as_ref().and_then(string_literal) {
            name = literal;
        }
        // Only the elements of `targets:` declare a target. A `.target(name:)`
        // inside `dependencies:` names one instead, and reading it would mint
        // a module the package does not build.
        if let Some(list) = argument(&args, "targets") {
            for element in list.children().filter(|c| c.kind() == "call_expression") {
                if let Some(target) = read_target(&element) {
                    targets.push(target);
                }
            }
        }
        break;
    }
    targets.sort_by(|a, b| a.name.cmp(&b.name));
    (name, targets)
}

/// One `.target(…)` element of a manifest's `targets:` list.
fn read_target(call: &SgNode) -> Option<Target> {
    let kind = TargetKind::from_factory(callee(call)?.as_str())?;
    let args = arguments(call);
    let name = argument(&args, "name").as_ref().and_then(string_literal)?;
    let dir = match argument(&args, "path").as_ref().and_then(string_literal) {
        Some(path) => normalize(&path),
        None => {
            let base = if kind == TargetKind::Test {
                DEFAULT_TEST_DIR
            } else {
                DEFAULT_SOURCE_DIR
            };
            format!("{base}/{name}")
        }
    };
    let mut excludes = literal_list(argument(&args, "exclude").as_ref());
    let mut sources = literal_list(argument(&args, "sources").as_ref());
    excludes.sort();
    sources.sort();
    Some(Target {
        name,
        kind,
        dir,
        excludes,
        sources,
    })
}

/// A repo-relative path with `./` and trailing slashes removed. `"."` is the
/// repository root and normalizes to `""`.
fn normalize(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed == "." {
        return String::new();
    }
    trimmed.strip_prefix("./").unwrap_or(trimmed).to_string()
}

/// The name a call names, without a leading dot: `Package`, `target`,
/// `testTarget`.
fn callee(call: &SgNode) -> Option<String> {
    let first = call.children().next()?;
    let text = first.text().to_string();
    Some(text.rsplit('.').next().unwrap_or(&text).trim().to_string())
}

/// A call's `value_argument` nodes, punctuation dropped.
fn arguments<'r>(call: &SgNode<'r>) -> Vec<SgNode<'r>> {
    let Some(suffix) = call.children().find(|c| c.kind() == "call_suffix") else {
        return Vec::new();
    };
    let Some(list) = suffix.children().find(|c| c.kind() == "value_arguments") else {
        return Vec::new();
    };
    list.children()
        .filter(|c| c.kind() == "value_argument")
        .collect()
}

/// The value of the argument carrying this label.
fn argument<'r>(args: &[SgNode<'r>], label: &str) -> Option<SgNode<'r>> {
    args.iter()
        .find(|a| {
            a.children()
                .next()
                .is_some_and(|l| l.kind() == "value_argument_label" && l.text() == label)
        })
        .and_then(|a| a.children().last())
}

/// Every plain string literal a node states: itself, or the members of an
/// array literal. Anything computed contributes nothing.
fn literal_list(node: Option<&SgNode>) -> Vec<String> {
    let Some(node) = node else { return Vec::new() };
    if let Some(one) = string_literal(node) {
        return vec![normalize(&one)];
    }
    node.children()
        .filter_map(|c| string_literal(&c))
        .map(|s| normalize(&s))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "// swift-tools-version: 6.3\nimport PackageDescription\n\
        let package = Package(name: \"Alamofire\",\n\
        \x20 products: [.library(name: \"Alamofire\", targets: [\"Alamofire\"])],\n\
        \x20 targets: [.target(name: \"Alamofire\", path: \"Source\", exclude: [\"Info.plist\"]),\n\
        \x20           .testTarget(name: \"AlamofireTests\", dependencies: [\"Alamofire\"],\n\
        \x20                       path: \"Tests\", exclude: [\"Info.plist\", \"Test Plans\"])])\n";

    #[test]
    fn a_manifest_states_its_package_and_its_targets() {
        let (name, targets) = read_manifest(MANIFEST);
        assert_eq!(name, "Alamofire");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].name, "Alamofire");
        assert_eq!(targets[0].kind, TargetKind::Regular);
        assert_eq!(targets[0].dir, "Source");
        assert_eq!(targets[0].excludes, ["Info.plist"]);
        assert_eq!(targets[1].name, "AlamofireTests");
        assert_eq!(targets[1].kind, TargetKind::Test);
        assert_eq!(targets[1].dir, "Tests");
        assert_eq!(targets[1].excludes, ["Info.plist", "Test Plans"]);
    }

    #[test]
    fn a_dependency_spelled_as_a_target_mints_no_module() {
        // `.target(name:)` inside `dependencies:` names a target; it does not
        // declare one. Reading it would put a module in the package that the
        // package does not build — and every import of a name that phantom
        // shadowed would resolve to nothing while looking resolved.
        let (_, targets) = read_manifest(
            "// swift-tools-version: 5.9\nlet package = Package(name: \"P\",\n\
             targets: [.target(name: \"Real\", dependencies: [.target(name: \"Phantom\")])])\n",
        );
        assert_eq!(
            targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["Real"],
        );
    }

    #[test]
    fn a_target_with_no_path_gets_swiftpms_default_directory() {
        let (_, targets) = read_manifest(
            "// swift-tools-version: 5.9\nlet package = Package(name: \"P\",\n\
             targets: [.target(name: \"Core\"), .testTarget(name: \"CoreTests\")])\n",
        );
        assert_eq!(targets[0].dir, "Sources/Core");
        assert_eq!(targets[1].dir, "Tests/CoreTests");
    }

    #[test]
    fn a_computed_target_list_leaves_the_layout_unknown_rather_than_empty() {
        // The guard against the cheapest way to raise a rate: with no target
        // read, "outside this package" is not a thing this build may assert,
        // so no import can be laundered into `External`.
        let (_, targets) = read_manifest(
            "// swift-tools-version: 5.9\nlet package = Package(name: \"P\", targets: allTargets)\n",
        );
        assert!(targets.is_empty());
        let package = SwiftPackage {
            targets,
            ..SwiftPackage::default()
        };
        assert!(!package.known());
    }

    #[test]
    fn the_newest_tools_version_wins_and_the_base_manifest_breaks_a_tie() {
        assert!(version_key("6.3") > version_key("6.2"));
        assert!(version_key("6.10") > version_key("6.9"));
        assert!(version_key("5.9.2") > version_key("5.9"));
        assert_eq!(tools_version("// swift-tools-version: 6.3\n"), "6.3");
        assert_eq!(tools_version("//swift-tools-version:5.0\n"), "5.0");
        assert_eq!(tools_version("import PackageDescription\n"), "");
    }

    #[test]
    fn four_manifests_resolve_to_the_newest_one() {
        let dir = tempfile::tempdir().expect("scratch");
        for (name, version) in [
            ("Package.swift", "6.3"),
            ("Package@swift-6.0.swift", "6.0"),
            ("Package@swift-6.1.swift", "6.1"),
            ("Package@swift-6.2.swift", "6.2"),
        ] {
            std::fs::write(
                dir.path().join(name),
                format!(
                    "// swift-tools-version: {version}\nlet package = Package(name: \"P\", \
                     targets: [.target(name: \"{}\", path: \"Source\")])\n",
                    name.replace(['.', '@', '-'], "_"),
                ),
            )
            .expect("manifest");
        }
        let cfg = layout(dir.path()).expect("layout");
        assert_eq!(cfg.manifest, "Package.swift");
        assert_eq!(cfg.tools_version, "6.3");
        assert_eq!(cfg.manifests.len(), 4);
        assert!(cfg.is_target("Package_swift"));
    }

    #[test]
    fn a_tree_with_no_manifest_has_no_module_namespace_at_all() {
        let dir = tempfile::tempdir().expect("scratch");
        let cfg = layout(dir.path()).expect("layout");
        assert!(!cfg.known());
        assert!(cfg.manifests.is_empty());
        // Every file is then its own module, which is the honest floor: a
        // file that belongs to no stated target belongs to no stated module.
        assert_eq!(cfg.module_fqn("a/b.swift"), "$a/b");
    }

    fn package(targets: Vec<Target>) -> SwiftPackage {
        SwiftPackage {
            name: "P".to_string(),
            manifests: vec!["Package.swift".to_string()],
            manifest: "Package.swift".to_string(),
            tools_version: "6.3".to_string(),
            targets,
        }
    }

    fn target(name: &str, kind: TargetKind, dir: &str, excludes: &[&str]) -> Target {
        Target {
            name: name.to_string(),
            kind,
            dir: dir.to_string(),
            excludes: excludes.iter().map(|e| (*e).to_string()).collect(),
            sources: Vec::new(),
        }
    }

    #[test]
    fn a_file_belongs_to_the_most_specific_target_that_claims_it() {
        let cfg = package(vec![
            target("Outer", TargetKind::Regular, "Source", &[]),
            target("Inner", TargetKind::Regular, "Source/Nested", &[]),
        ]);
        assert_eq!(cfg.module_fqn("Source/Core/Session.swift"), "Outer");
        assert_eq!(cfg.module_fqn("Source/Nested/A.swift"), "Inner");
    }

    #[test]
    fn an_excluded_file_belongs_to_no_target_and_a_manifest_belongs_to_none_either() {
        let cfg = package(vec![
            target("Alamofire", TargetKind::Regular, "Source", &["Info.plist"]),
            target("AlamofireTests", TargetKind::Test, "Tests", &["Test Plans"]),
        ]);
        assert_eq!(cfg.module_fqn("Source/Alamofire.swift"), "Alamofire");
        assert_eq!(cfg.module_fqn("Tests/SessionTests.swift"), "AlamofireTests");
        assert_eq!(
            cfg.module_fqn("Tests/Test Plans/x.swift"),
            "$Tests/Test Plans/x"
        );
        // The manifests sit at the root, under no target's directory: each is
        // its own module, exactly as SwiftPM compiles them.
        assert_eq!(cfg.module_fqn("Package.swift"), "$Package");
        assert_eq!(
            cfg.module_fqn("Package@swift-6.0.swift"),
            "$Package@swift-6.0",
        );
    }

    #[test]
    fn an_explicit_source_list_takes_only_what_it_names() {
        let mut t = target("Core", TargetKind::Regular, "Source", &[]);
        t.sources = vec!["A.swift".to_string(), "Sub".to_string()];
        let cfg = package(vec![t]);
        assert_eq!(cfg.module_fqn("Source/A.swift"), "Core");
        assert_eq!(cfg.module_fqn("Source/Sub/B.swift"), "Core");
        assert_eq!(cfg.module_fqn("Source/C.swift"), "$Source/C");
    }

    #[test]
    fn the_digest_moves_when_the_layout_does_and_not_otherwise() {
        let a = package(vec![target("Core", TargetKind::Regular, "Source", &[])]);
        let b = a.clone();
        assert_eq!(a.digest(), b.digest());
        let mut c = a.clone();
        c.targets[0].dir = "Sources/Core".to_string();
        assert_ne!(a.digest(), c.digest());
        // Adding a manifest can change which one is read, so it must re-root
        // the graph rather than silently change nothing.
        let mut d = a.clone();
        d.manifests.push("Package@swift-6.0.swift".to_string());
        assert_ne!(a.digest(), d.digest());
        // A target name and a directory cannot be confused for one another.
        let e = package(vec![target("Source", TargetKind::Regular, "Core", &[])]);
        assert_ne!(a.digest(), e.digest());
    }
}
