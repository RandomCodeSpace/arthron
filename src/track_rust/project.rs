//! Rust's project layout: which files are crate roots, which package owns
//! them, and what each package declares as a dependency.
//!
//! Everything downstream depends on this module. A Rust module's name is a
//! fact about *where its file sits relative to a crate root*, and a crate
//! root is a fact stated in `Cargo.toml` rather than in any `.rs` file. Get
//! this wrong and nothing links.
//!
//! # Why a directory under `crates/` is not a crate
//!
//! The measured corpus makes the point on its own: `crates/core` is the
//! largest directory in ripgrep — a quarter of the snapshot — and it has no
//! `Cargo.toml`. It compiles as the root package's binary, reachable only by
//! reading `[[bin]] path = "crates/core/main.rs"` out of the root manifest.
//! So targets are read, never inferred from the directory tree.
//!
//! # The five ways a file becomes a crate root
//!
//! `[lib]`/`src/lib.rs`, `[[bin]]`/`src/main.rs`/`src/bin/*.rs`, `[[test]]`/
//! `tests/*.rs`, `[[example]]`/`examples/*.rs`, `[[bench]]`/`benches/*.rs` —
//! plus the build script, which is a crate of its own. Auto-discovery is
//! switched off per kind by `autobins`/`autotests`/`autoexamples`/
//! `autobenches`, and the corpus exercises that: two manifests set
//! `autotests = false` and name their integration test explicitly, so
//! `tests/util.rs` beside it is a *module*, not a root.
//!
//! # What is deliberately not modelled
//!
//! - **`#[path = "…"]`.** The corpus contains none, so the one mechanism that
//!   detaches a module's name from its conventional file path is unexercised
//!   here. Implementing it blind would be guessing; a module's file is
//!   therefore its conventional path, and a second corpus or a probe is what
//!   earns the attribute.
//! - **Editions before 2018.** Every manifest in the corpus is `edition =
//!   "2024"`. Under 2018 and later a bare first segment in a `use` path is a
//!   crate name; under 2015 it could also be a top-level module of the
//!   current crate. Only the modern rule is implemented, and a 2015 crate
//!   would misreport rather than silently guess — recorded here so the
//!   shortfall is a known one.
//! - **Feature and target resolution.** 40 of the corpus's `mod` declarations
//!   sit under a `#[cfg(…)]`, so the module tree is a function of features
//!   and target platform. arthron reads every `mod` declaration regardless:
//!   the union over configurations is the honest superset, and a `mod` whose
//!   file is absent under *every* configuration still misses.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Crates that ship with the toolchain rather than with the repository.
///
/// A `use` rooted at one of these is `External` without consulting any
/// manifest, because no manifest declares them.
pub const SYSROOT_CRATES: &[&str] = &["std", "core", "alloc", "proc_macro", "test"];

/// What kind of target a crate root is.
///
/// Kept because two roots can share a directory — `crates/ignore/tests/` holds
/// two auto-discovered integration tests — and because the kind is the only
/// thing distinguishing a package's library, which an `extern crate` name
/// reaches, from its binaries, which nothing outside it can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetKind {
    /// The package's library. The only target another crate can name.
    Lib,
    /// An executable.
    Bin,
    /// An integration test.
    Test,
    /// An example.
    Example,
    /// A benchmark.
    Bench,
    /// The build script.
    Build,
}

impl TargetKind {
    /// The kind's name, for diagnostics and the config fingerprint.
    pub fn name(self) -> &'static str {
        match self {
            TargetKind::Lib => "lib",
            TargetKind::Bin => "bin",
            TargetKind::Test => "test",
            TargetKind::Example => "example",
            TargetKind::Bench => "bench",
            TargetKind::Build => "build",
        }
    }
}

/// One crate root: a file that is the top of its own module tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Repo-relative, `/`-separated path of the root file.
    pub root: String,
    /// Index into [`RsWorkspace::packages`] of the package that declares it.
    pub package: usize,
    /// What kind of target it is.
    pub kind: TargetKind,
}

impl Target {
    /// The directory the target's module tree is rooted at: everything under
    /// it, minus the roots themselves, is a module of this crate.
    pub fn module_dir(&self) -> &str {
        match self.root.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        }
    }
}

/// Where a dependency's source lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dep {
    /// A sibling package in this repository, named by its directory. The
    /// corpus has 20 of these `path = …` edges across ten manifests, so a
    /// crate name really does resolve to a directory rather than a registry.
    Local(String),
    /// A dependency outside this repository.
    External,
}

/// One Cargo package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Repo-relative directory holding the manifest. `""` is the repository
    /// root.
    pub dir: String,
    /// The declared package name, or `""` for a manifest with no `[package]`
    /// — a virtual workspace root.
    pub name: String,
    /// Declared dependencies, keyed by the name *source code uses*. A
    /// `package = "…"` rename means the key and the registry name differ, and
    /// the key is what a `use` path can name — the corpus has one:
    /// `memmap = { package = "memmap2", … }`.
    pub deps: BTreeMap<String, Dep>,
}

/// The workspace a Rust scan resolves against.
///
/// This is `RsLang`'s configuration: the driver moves it between phases and
/// never inspects it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RsWorkspace {
    /// Every package that owns at least one walked `.rs` file, sorted by
    /// directory.
    pub packages: Vec<Package>,
    /// Every crate root, sorted by path.
    pub targets: Vec<Target>,
}

/// Where one file sits in the module namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModPlace {
    /// The crate root this file belongs to, repo-relative. A file under no
    /// target is its own root, which is what a stray `.rs` file no manifest
    /// mentions really is.
    pub crate_root: String,
    /// Module segments below the crate root, outermost first. Empty when the
    /// file *is* the crate root.
    pub segments: Vec<String>,
}

impl RsWorkspace {
    /// Read every manifest that governs a walked file.
    ///
    /// The walk hands over `.rs` paths only, so the manifests are found by
    /// climbing from each of them: a package with no Rust file of its own
    /// governs nothing this scan reads and is not a fact this scan needs.
    pub fn load(root: &Path, files: &[String]) -> RsWorkspace {
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for file in files {
            let mut dir = match file.rsplit_once('/') {
                Some((d, _)) => d.to_string(),
                None => String::new(),
            };
            loop {
                if root.join(manifest_path(&dir)).is_file() {
                    dirs.insert(dir.clone());
                }
                match dir.rsplit_once('/') {
                    Some((parent, _)) => dir = parent.to_string(),
                    None if dir.is_empty() => break,
                    None => dir = String::new(),
                }
            }
        }

        let mut packages = Vec::with_capacity(dirs.len());
        let mut targets = Vec::new();
        for dir in &dirs {
            let text = std::fs::read_to_string(root.join(manifest_path(dir))).unwrap_or_default();
            let table: toml::Table = text.parse().unwrap_or_default();
            let index = packages.len();
            packages.push(Package {
                dir: dir.clone(),
                name: str_at(&table, &["package", "name"]).unwrap_or_default(),
                deps: dependencies(&table, dir),
            });
            collect_targets(root, &table, dir, index, &mut targets);
        }
        targets.sort_by(|a, b| (&a.root, a.kind).cmp(&(&b.root, b.kind)));
        targets.dedup_by(|a, b| a.root == b.root);
        RsWorkspace { packages, targets }
    }

    /// Where a file sits in the module namespace.
    ///
    /// A file that *is* a crate root is that crate's top module. Otherwise the
    /// owning target is the one whose module directory is the longest prefix
    /// of the file's, which is what makes `crates/core/flags/complete/bash.rs`
    /// a module of the root package's binary rather than of any library.
    pub fn place(&self, rel_path: &str) -> ModPlace {
        if let Some(t) = self.targets.iter().find(|t| t.root == rel_path) {
            return ModPlace {
                crate_root: t.root.clone(),
                segments: Vec::new(),
            };
        }
        let dir = match rel_path.rsplit_once('/') {
            Some((d, _)) => d,
            None => "",
        };
        let owner = self
            .targets
            .iter()
            .filter(|t| dir_contains(t.module_dir(), dir))
            .max_by_key(|t| t.module_dir().len());
        match owner {
            Some(t) => ModPlace {
                crate_root: t.root.clone(),
                segments: module_segments(t.module_dir(), rel_path),
            },
            // No target reaches it: the file is not part of any crate this
            // manifest set describes. It is named by its own path, which
            // collides with no crate root — every root is a walked `.rs` file
            // and this one is not one of them.
            None => ModPlace {
                crate_root: rel_path.to_string(),
                segments: Vec::new(),
            },
        }
    }

    /// The module FQN of a file: its crate root, then one `::` per segment.
    pub fn module_fqn(&self, rel_path: &str) -> String {
        let place = self.place(rel_path);
        join_module(&place.crate_root, &place.segments)
    }

    /// The package that owns a file, by the longest manifest directory above
    /// it. `None` only when no manifest governs the file at all.
    pub fn package_of(&self, rel_path: &str) -> Option<usize> {
        let dir = match rel_path.rsplit_once('/') {
            Some((d, _)) => d,
            None => "",
        };
        self.packages
            .iter()
            .enumerate()
            .filter(|(_, p)| dir_contains(&p.dir, dir))
            .max_by_key(|(_, p)| p.dir.len())
            .map(|(i, _)| i)
    }

    /// The library crate root a package exposes: the only one of its targets
    /// another crate's `use` path can name.
    pub fn lib_root(&self, package: usize) -> Option<&str> {
        self.targets
            .iter()
            .find(|t| t.package == package && t.kind == TargetKind::Lib)
            .map(|t| t.root.as_str())
    }

    /// The package directory a `path = …` dependency points at.
    pub fn package_at(&self, dir: &str) -> Option<usize> {
        self.packages.iter().position(|p| p.dir == dir)
    }

    /// A fingerprint of everything the manifests decide.
    ///
    /// Every crate root and every dependency edge: the first roots every FQN
    /// in the graph, the second decides whether a `use` reaches a sibling
    /// crate, an outside dependency, or nothing at all.
    pub fn digest(&self) -> Vec<u8> {
        let mut hasher = blake3::Hasher::new();
        let mut field = |bytes: &[u8]| {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(&(self.packages.len() as u64).to_le_bytes());
        for package in &self.packages {
            field(package.dir.as_bytes());
            field(package.name.as_bytes());
            field(&(package.deps.len() as u64).to_le_bytes());
            for (name, dep) in &package.deps {
                field(name.as_bytes());
                match dep {
                    Dep::Local(dir) => field(format!("path:{dir}").as_bytes()),
                    Dep::External => field(b"external"),
                }
            }
        }
        field(&(self.targets.len() as u64).to_le_bytes());
        for target in &self.targets {
            field(target.root.as_bytes());
            field(target.kind.name().as_bytes());
            field(&(target.package as u64).to_le_bytes());
        }
        hasher.finalize().as_bytes().to_vec()
    }
}

/// Render a crate root and its module segments as one FQN.
///
/// `::` joins module segments and never appears in a path, so a module name
/// and a crate root can never collide; `#`, which separates a container from
/// its members, appears in neither.
pub fn join_module(crate_root: &str, segments: &[String]) -> String {
    let mut out = String::from(crate_root);
    for segment in segments {
        out.push_str("::");
        out.push_str(segment);
    }
    out
}

/// The parent of a module FQN, or `None` at a crate root — `super` from the
/// top of a crate names nothing, and inventing a parent would resolve it.
pub fn parent_module(fqn: &str) -> Option<&str> {
    fqn.rsplit_once("::").map(|(parent, _)| parent)
}

/// A file's module segments below a target's module directory.
///
/// `foo.rs` is module `foo`; `foo/mod.rs` is module `foo`, not `foo::mod`.
/// Those are the two file shapes a module name can take, and the corpus uses
/// both.
fn module_segments(module_dir: &str, rel_path: &str) -> Vec<String> {
    let rest = if module_dir.is_empty() {
        rel_path
    } else {
        &rel_path[module_dir.len() + 1..]
    };
    let rest = rest.strip_suffix(".rs").unwrap_or(rest);
    let mut segments: Vec<String> = rest.split('/').map(str::to_string).collect();
    if segments.last().is_some_and(|s| s == "mod") {
        segments.pop();
    }
    segments
}

/// Whether `dir` is `outer` or sits under it. Component-wise, so `crates/co`
/// does not contain `crates/core`.
fn dir_contains(outer: &str, dir: &str) -> bool {
    if outer.is_empty() {
        return true;
    }
    dir == outer
        || (dir.len() > outer.len()
            && dir.starts_with(outer)
            && dir.as_bytes()[outer.len()] == b'/')
}

fn manifest_path(dir: &str) -> String {
    if dir.is_empty() {
        "Cargo.toml".to_string()
    } else {
        format!("{dir}/Cargo.toml")
    }
}

/// Join a package directory and a manifest-relative path.
fn under(dir: &str, rel: &str) -> String {
    let rel = rel.replace('\\', "/");
    if dir.is_empty() {
        rel
    } else {
        format!("{dir}/{rel}")
    }
}

/// Normalise a `path = "../cli"`-style dependency directory against the
/// package that declared it.
fn normalise(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    let rel = rel.replace('\\', "/");
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn str_at(table: &toml::Table, path: &[&str]) -> Option<String> {
    let mut value: &toml::Value = table.get(path[0])?;
    for key in &path[1..] {
        value = value.as_table()?.get(*key)?;
    }
    value.as_str().map(str::to_string)
}

fn bool_at(table: &toml::Table, path: &[&str]) -> Option<bool> {
    let mut value: &toml::Value = table.get(path[0])?;
    for key in &path[1..] {
        value = value.as_table()?.get(*key)?;
    }
    value.as_bool()
}

/// Every dependency a manifest declares, from all four places one can sit:
/// `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, and the
/// per-target tables under `[target.'cfg(…)']`. All four bind a name a `use`
/// path may root at, so leaving one out turns a real dependency into an
/// unknown package.
fn dependencies(table: &toml::Table, dir: &str) -> BTreeMap<String, Dep> {
    let mut out = BTreeMap::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(t) = table.get(key).and_then(toml::Value::as_table) {
            collect_deps(t, dir, &mut out);
        }
    }
    if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
        for (_, cfg) in targets {
            let Some(cfg) = cfg.as_table() else { continue };
            for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(t) = cfg.get(key).and_then(toml::Value::as_table) {
                    collect_deps(t, dir, &mut out);
                }
            }
        }
    }
    out
}

fn collect_deps(table: &toml::Table, dir: &str, out: &mut BTreeMap<String, Dep>) {
    for (name, spec) in table {
        // The *key* is the name source code uses. `package = "memmap2"` under
        // the key `memmap` renames the crate, and `use memmap::…` is what the
        // source then writes.
        let dep = match spec
            .as_table()
            .and_then(|t| t.get("path"))
            .and_then(toml::Value::as_str)
        {
            Some(path) => Dep::Local(normalise(dir, path)),
            None => Dep::External,
        };
        out.insert(name.replace('-', "_"), dep);
    }
}

/// Every crate root one manifest declares or auto-discovers.
fn collect_targets(
    root: &Path,
    table: &toml::Table,
    dir: &str,
    package: usize,
    out: &mut Vec<Target>,
) {
    let mut push = |rel: String, kind: TargetKind| {
        if root.join(&rel).is_file() {
            out.push(Target {
                root: rel,
                package,
                kind,
            });
        }
    };

    // The library. `[lib] path` overrides the convention; otherwise
    // `src/lib.rs` is a library when it exists and nothing when it does not,
    // which is how the root package here has no library at all.
    match str_at(table, &["lib", "path"]) {
        Some(path) => push(under(dir, &path), TargetKind::Lib),
        None => push(under(dir, "src/lib.rs"), TargetKind::Lib),
    }

    // The build script: a crate of its own, compiled and run before the rest.
    match table
        .get("package")
        .and_then(|p| p.as_table())
        .and_then(|p| p.get("build"))
    {
        Some(toml::Value::String(path)) => push(under(dir, path), TargetKind::Build),
        Some(toml::Value::Boolean(false)) => {}
        _ => push(under(dir, "build.rs"), TargetKind::Build),
    }

    for (key, auto, kind, conventional) in [
        ("bin", "autobins", TargetKind::Bin, "src/bin"),
        ("test", "autotests", TargetKind::Test, "tests"),
        ("example", "autoexamples", TargetKind::Example, "examples"),
        ("bench", "autobenches", TargetKind::Bench, "benches"),
    ] {
        if let Some(array) = table.get(key).and_then(toml::Value::as_array) {
            for entry in array {
                if let Some(path) = entry
                    .as_table()
                    .and_then(|t| t.get("path"))
                    .and_then(toml::Value::as_str)
                {
                    push(under(dir, path), kind);
                }
            }
        }
        // Auto-discovery is on unless the manifest switches it off — which
        // the corpus does twice, and which is the difference between
        // `tests/util.rs` being a module of the integration test and being a
        // crate root of its own.
        if bool_at(table, &["package", auto]) == Some(false) {
            continue;
        }
        if kind == TargetKind::Bin {
            push(under(dir, "src/main.rs"), TargetKind::Bin);
        }
        let conventional_dir = root.join(under(dir, conventional));
        let Ok(entries) = std::fs::read_dir(&conventional_dir) else {
            continue;
        };
        let mut found: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_file() && name.ends_with(".rs") {
                found.push(under(dir, &format!("{conventional}/{name}")));
            } else if entry.path().is_dir() && entry.path().join("main.rs").is_file() {
                found.push(under(dir, &format!("{conventional}/{name}/main.rs")));
            }
        }
        found.sort();
        for path in found {
            push(path, kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_a_module_of_the_target_whose_directory_holds_it() {
        assert_eq!(
            module_segments("crates/core", "crates/core/flags/complete/bash.rs"),
            ["flags", "complete", "bash"]
        );
        // `foo/mod.rs` is module `foo`, never `foo::mod`.
        assert_eq!(
            module_segments("crates/core", "crates/core/flags/mod.rs"),
            ["flags"]
        );
        assert_eq!(module_segments("", "tests/util.rs"), ["tests", "util"]);
    }

    #[test]
    fn a_directory_prefix_is_component_wise() {
        assert!(dir_contains("crates/core", "crates/core/flags"));
        assert!(dir_contains("crates/core", "crates/core"));
        assert!(dir_contains("", "anything/at/all"));
        assert!(!dir_contains("crates/co", "crates/core"));
        assert!(!dir_contains("crates/core", "crates"));
    }

    #[test]
    fn a_relative_dependency_path_is_resolved_against_its_manifest() {
        assert_eq!(normalise("crates/grep", "../cli"), "crates/cli");
        assert_eq!(normalise("", "crates/ignore"), "crates/ignore");
        assert_eq!(normalise("a/b", "./c"), "a/b/c");
    }

    #[test]
    fn a_module_fqn_joins_the_crate_root_with_double_colons() {
        assert_eq!(
            join_module("crates/x/src/lib.rs", &[]),
            "crates/x/src/lib.rs"
        );
        assert_eq!(
            join_module("crates/x/src/lib.rs", &["a".into(), "b".into()]),
            "crates/x/src/lib.rs::a::b"
        );
        assert_eq!(
            parent_module("crates/x/src/lib.rs::a::b"),
            Some("crates/x/src/lib.rs::a")
        );
        // `super` at a crate root names nothing, and nothing invents one.
        assert_eq!(parent_module("crates/x/src/lib.rs"), None);
    }

    #[test]
    fn a_dependency_key_is_the_name_source_code_writes() {
        let table: toml::Table = r#"
[dependencies]
memmap = { package = "memmap2", version = "0.9.0" }
grep-matcher = { version = "0.1.8", path = "../matcher" }
log = "0.4"
"#
        .parse()
        .expect("parses");
        let deps = dependencies(&table, "crates/searcher");
        // The rename's key is what `use memmap::…` writes, not `memmap2`.
        assert_eq!(deps.get("memmap"), Some(&Dep::External));
        assert!(!deps.contains_key("memmap2"));
        // A hyphen in a package name is an underscore in source.
        assert_eq!(
            deps.get("grep_matcher"),
            Some(&Dep::Local("crates/matcher".into()))
        );
        assert_eq!(deps.get("log"), Some(&Dep::External));
    }

    #[test]
    fn dependencies_come_from_all_four_tables() {
        let table: toml::Table = r#"
[dependencies]
a = "1"
[dev-dependencies]
b = "1"
[build-dependencies]
c = "1"
[target.'cfg(windows)'.dependencies]
d = "1"
"#
        .parse()
        .expect("parses");
        let deps = dependencies(&table, "");
        for name in ["a", "b", "c", "d"] {
            assert!(deps.contains_key(name), "{name} was not read");
        }
    }
}
