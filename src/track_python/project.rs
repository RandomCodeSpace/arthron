//! Python's project layout: what one `go.mod` line is to Go, spread over four
//! packaging formats and one filesystem convention.
//!
//! Everything downstream depends on this module. A module's name is a fact
//! about *where its file is*, every FQN in the graph is rooted at a module
//! name, and Python states that fact in packaging metadata rather than in the
//! source (A-03/A-05). Getting it wrong does not produce wrong edges; it
//! produces a graph in which nothing links, so this is the highest-leverage
//! piece of Python plumbing and the one worth reading first.
//!
//! # The module namespace
//!
//! Three disjoint shapes, and the disjointness is load-bearing — two files
//! sharing a module FQN share a [`crate::model::NodeId`], and their
//! definitions silently merge:
//!
//! | Shape | Example | When |
//! |---|---|---|
//! | dotted | `pkg.sub` | the file sits under exactly one package root |
//! | root-prefixed | `src/pkg.sub` | the project has more than one root (A-06) |
//! | path | `tests/a/test_utils.py` | the file sits under no package (A-07) |
//!
//! A Python identifier contains no `/` and no `.`, so a dotted name can never
//! equal a root-prefixed one. Every walked file ends in `.py`, and no dotted
//! or root-prefixed name ends in `.py` — a trailing `py` segment would have to
//! be preceded by `.`, and [`is_identifier`] rejects an empty segment — so a
//! path name can never equal either. The three shapes therefore partition,
//! and the mapping from file to module FQN is injective by construction
//! rather than by hope.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

/// The project layout a Python scan resolves against.
///
/// This is `PyLang`'s configuration: the driver moves it between phases and
/// never inspects it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PyProject {
    /// Package roots, repo-relative and `/`-separated, sorted and unique.
    /// `""` is the repository root. A module's dotted name is relative to the
    /// root that owns it, and the root itself is never part of the name.
    pub roots: Vec<String>,
    /// Whether [`PyProject::roots`] came from packaging metadata rather than
    /// from the `__init__.py` walk. Declared roots are authoritative: they are
    /// the only way to tell a PEP 420 namespace package from a directory that
    /// is the root (A-04).
    pub declared: bool,
    /// Repo-relative directories holding an `__init__.py` (A-03).
    pub packages: HashSet<String>,
    /// Declared third-party distribution names, normalised (B-23).
    pub dependencies: BTreeSet<String>,
    /// Dotted module names of compiled extension modules — `.so`, `.pyd`,
    /// `.dll` (A-10). Real modules arthron will never parse, so a reference
    /// into one is `External` rather than a missing definition.
    pub ext_modules: BTreeSet<String>,
}

/// Where one file sits in the module namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModPlace {
    /// Under a package root: a dotted name relative to that root.
    Rooted {
        /// The owning root, repo-relative. `""` is the repository root.
        root: String,
        /// The dotted module name, root-relative.
        dotted: String,
    },
    /// Under no package. Named by its repo-relative path, which collides with
    /// nothing (A-07/A-11).
    Loose {
        /// The repo-relative path, `/`-separated.
        rel_path: String,
    },
}

impl PyProject {
    /// The module FQN for one file. See the module docs for the grammar.
    pub fn module_fqn(&self, rel_path: &str) -> String {
        self.fqn_of(&self.place(rel_path))
    }

    /// Render a [`ModPlace`] as an FQN string.
    pub fn fqn_of(&self, place: &ModPlace) -> String {
        match place {
            ModPlace::Rooted { root, dotted } if self.multi_root() => {
                format!("{}/{dotted}", root_prefix(root))
            }
            ModPlace::Rooted { dotted, .. } => dotted.clone(),
            ModPlace::Loose { rel_path } => rel_path.clone(),
        }
    }

    /// Whether the project has more than one package root, which is the only
    /// condition under which a module FQN carries its root (A-06).
    pub fn multi_root(&self) -> bool {
        self.roots.len() > 1
    }

    /// Every FQN a dotted module name could have, the importing file's own
    /// root first (A-06).
    ///
    /// One name, several answers: nothing stops two distributions in one tree
    /// from exporting the same top-level package, and their modules are
    /// different nodes. An ordered candidate list is exactly the shape the
    /// core was built around, so this needs no model change.
    pub fn module_fqns(&self, own_root: &str, dotted: &str) -> Vec<String> {
        if !self.multi_root() {
            return vec![dotted.to_string()];
        }
        let mut out = Vec::with_capacity(self.roots.len());
        out.push(format!("{}/{dotted}", root_prefix(own_root)));
        for root in &self.roots {
            let candidate = format!("{}/{dotted}", root_prefix(root));
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
        out
    }

    /// Where a file sits in the module namespace.
    pub fn place(&self, rel_path: &str) -> ModPlace {
        let loose = || ModPlace::Loose {
            rel_path: rel_path.to_string(),
        };
        let (dir, file) = match rel_path.rsplit_once('/') {
            Some((d, f)) => (d, f),
            None => ("", rel_path),
        };
        let stem = file.strip_suffix(".py").unwrap_or(file);
        let Some(root) = self.owning_root(dir) else {
            return loose();
        };
        let mut segments: Vec<&str> = Vec::new();
        // The root itself is not part of the name; §5.2.1 makes the name
        // relative to a `sys.path` entry, not to the repository.
        let below = dir.strip_prefix(root.as_str()).unwrap_or(dir);
        segments.extend(below.split('/').filter(|s| !s.is_empty()));
        // A-01: `pkg/__init__.py` *is* the module `pkg`. There is no
        // `pkg.__init__` node, and inventing one makes every
        // `from pkg import X` miss.
        if stem != "__init__" {
            segments.push(stem);
        }
        if segments.is_empty() || !segments.iter().all(|s| is_identifier(s)) {
            return loose();
        }
        ModPlace::Rooted {
            root,
            dotted: segments.join("."),
        }
    }

    /// The root that owns a directory, or `None` when no package claims it.
    ///
    /// Declared roots win outright: they are the only input that can tell a
    /// PEP 420 namespace package from the root above it (A-04), and the
    /// `__init__.py` walk stops one level too early on one.
    fn owning_root(&self, dir: &str) -> Option<String> {
        if self.declared {
            return self
                .roots
                .iter()
                .filter(|root| dir_under(dir, root))
                .max_by_key(|root| root.len())
                .cloned();
        }
        // A-03: walk up while each directory is a package; the first that is
        // not is the root. A file whose own directory holds no `__init__.py`
        // is in no package at all (A-07).
        if !self.packages.contains(dir) {
            return None;
        }
        let mut current = dir;
        loop {
            let Some((parent, _)) = current.rsplit_once('/') else {
                return Some(String::new()); // the repository root
            };
            if !self.packages.contains(parent) {
                return Some(parent.to_string());
            }
            current = parent;
        }
    }

    /// Whether a top-level import name belongs to a declared dependency.
    pub fn declares_dependency(&self, top: &str) -> bool {
        self.dependencies.contains(&normalise_dist(top))
    }

    /// A fingerprint of everything the *manifest* decides.
    ///
    /// Roots root every FQN in the graph and dependencies decide whether an
    /// import is a known dependency or an unknown package, so a store built
    /// under different ones describes a different project. `packages` is
    /// deliberately absent: it is a walk fact that moves whenever a file does,
    /// and folding it in would wipe the store on every scan.
    pub fn digest(&self) -> Vec<u8> {
        let mut hasher = blake3::Hasher::new();
        let mut field = |bytes: &[u8]| {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(&[u8::from(self.declared)]);
        field(&(self.roots.len() as u64).to_le_bytes());
        for root in &self.roots {
            field(root.as_bytes());
        }
        field(&(self.dependencies.len() as u64).to_le_bytes());
        for dep in &self.dependencies {
            field(dep.as_bytes());
        }
        for module in &self.ext_modules {
            field(module.as_bytes());
        }
        hasher.finalize().as_bytes().to_vec()
    }
}

/// How a root appears inside an FQN. The repository root is written `.`,
/// because an empty prefix would make `/pkg.sub` and leave the reader
/// guessing whether a segment went missing.
fn root_prefix(root: &str) -> &str {
    if root.is_empty() { "." } else { root }
}

/// Whether `dir` is `root` or sits beneath it.
fn dir_under(dir: &str, root: &str) -> bool {
    root.is_empty() || dir == root || dir.starts_with(&format!("{root}/"))
}

/// Whether a path segment is a Python identifier, and therefore able to be
/// part of a dotted module name (§2.3).
///
/// The FQN grammar's guard rail: a segment that is not an identifier cannot be
/// imported by name, and letting one through is how a directory called
/// `my-pkg` or `a#b` would forge an FQN in someone else's namespace.
pub fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_alphabetic() || first == '_') && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// PEP 503 name normalisation, as far as an import name can be compared to a
/// distribution name at all.
///
/// The two are genuinely different namespaces — `PyYAML` installs `yaml` — so
/// this is a *conservative* comparison: a match means the import is a declared
/// dependency, and a miss means arthron does not know, which reports as
/// `UnknownPackage` and counts against the rate rather than quietly leaving
/// it.
pub fn normalise_dist(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '-' | '.' => '_',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Manifests
// ---------------------------------------------------------------------------

/// One `key = value` assignment, with the section it sits under.
struct Assignment {
    section: String,
    key: String,
    value: String,
}

/// A deliberately small TOML reader: section headers and `key = value`, with
/// bracketed values joined across lines.
///
/// The direct analogue of [`crate::resolve_go::parse_go_mod`], and small for
/// the same reason: four keys are read out of `pyproject.toml`, none of them
/// needs a type system, and a dependency that parses all of TOML would be a
/// dependency added for four keys.
fn toml_assignments(src: &str) -> Vec<Assignment> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut pending: Option<(String, String)> = None;
    for line in src.lines() {
        let line = strip_comment(line);
        let trimmed = line.trim();
        if let Some((key, buffer)) = pending.as_mut() {
            buffer.push(' ');
            buffer.push_str(trimmed);
            if balanced(buffer) {
                out.push(Assignment {
                    section: section.clone(),
                    key: std::mem::take(key),
                    value: std::mem::take(buffer),
                });
                pending = None;
            }
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed
                .trim_matches(|c| c == '[' || c == ']')
                .trim()
                .to_string();
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches('"').to_string();
        let value = value.trim().to_string();
        if key.is_empty() {
            continue;
        }
        if balanced(&value) {
            out.push(Assignment {
                section: section.clone(),
                key,
                value,
            });
        } else {
            pending = Some((key, value));
        }
    }
    out
}

/// Whether every `[` and `{` in a value is closed. Quoted text is skipped, so
/// a bracket inside a requirement string does not hold the reader open.
fn balanced(value: &str) -> bool {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in value.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth -= 1,
            _ => {}
        }
    }
    depth <= 0
}

/// Drop a `#` comment, respecting quotes — a requirement may carry a URL
/// fragment (`pkg @ https://host/a.zip#sha256=…`).
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '#') => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Every quoted string in a value, in order.
fn quoted_strings(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<(char, String)> = None;
    for c in value.chars() {
        match &mut current {
            Some((quote, buffer)) => {
                if c == *quote {
                    out.push(std::mem::take(buffer));
                    current = None;
                } else {
                    buffer.push(c);
                }
            }
            None if c == '"' || c == '\'' => current = Some((c, String::new())),
            None => {}
        }
    }
    out
}

/// The distribution name at the head of a PEP 508 requirement.
fn requirement_name(spec: &str) -> Option<String> {
    let name: String = spec
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect();
    (!name.is_empty()).then(|| normalise_dist(&name))
}

/// Normalise a declared root: no trailing slash, no `./` prefix, `.` is the
/// repository root.
fn normalise_root(dir: &str) -> String {
    let dir = dir.trim().trim_end_matches('/');
    let dir = dir.strip_prefix("./").unwrap_or(dir);
    if dir == "." {
        String::new()
    } else {
        dir.to_string()
    }
}

/// Roots and dependencies declared by a `pyproject.toml` (A-05, B-23).
pub fn parse_pyproject(src: &str) -> (Vec<String>, BTreeSet<String>) {
    let mut roots = Vec::new();
    let mut deps = BTreeSet::new();
    for a in toml_assignments(src) {
        match (a.section.as_str(), a.key.as_str()) {
            // setuptools: `[tool.setuptools.packages.find] where = ["src"]`
            ("tool.setuptools.packages.find", "where") => {
                roots.extend(quoted_strings(&a.value).iter().map(|d| normalise_root(d)));
            }
            // setuptools: `[tool.setuptools] package-dir = {"" = "src"}`
            ("tool.setuptools", "package-dir" | "package_dir") => {
                let parts = quoted_strings(&a.value);
                for pair in parts.chunks(2) {
                    if let [key, value] = pair
                        && key.is_empty()
                    {
                        roots.push(normalise_root(value));
                    }
                }
            }
            // poetry: `packages = [{include = "x", from = "src"}]`
            ("tool.poetry", "packages") => roots.extend(poetry_package_roots(&a.value)),
            // hatch: `packages = ["src/x"]`
            ("tool.hatch.build.targets.wheel", "packages") => {
                for path in quoted_strings(&a.value) {
                    roots.push(match path.trim_end_matches('/').rsplit_once('/') {
                        Some((parent, _)) => normalise_root(parent),
                        None => String::new(),
                    });
                }
            }
            ("project", "dependencies") => {
                deps.extend(
                    quoted_strings(&a.value)
                        .iter()
                        .filter_map(|s| requirement_name(s)),
                );
            }
            ("project.optional-dependencies", _) => {
                deps.extend(
                    quoted_strings(&a.value)
                        .iter()
                        .filter_map(|s| requirement_name(s)),
                );
            }
            // poetry states dependencies as keys, not as requirement strings.
            ("tool.poetry.dependencies" | "tool.poetry.group.dev.dependencies", key)
                if key != "python" =>
            {
                deps.insert(normalise_dist(key));
            }
            _ => {}
        }
    }
    (roots, deps)
}

/// The `from = "…"` of each poetry package entry; an entry without one is
/// relative to the project root.
fn poetry_package_roots(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in value.split('{').skip(1) {
        let chunk = chunk.split('}').next().unwrap_or(chunk);
        let root = match chunk.split_once("from") {
            Some((_, rest)) => quoted_strings(rest)
                .first()
                .map_or_else(String::new, |d| normalise_root(d)),
            None => String::new(),
        };
        out.push(root);
    }
    out
}

/// Roots declared by a `setup.py`'s `package_dir` (A-05).
///
/// Textual on purpose: `setup.py` is a program, and running it to find out
/// where the packages are would be the one thing this project never does.
pub fn parse_setup_py(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in src.match_indices("package_dir") {
        let rest = &src[i..];
        let Some(open) = rest.find('{') else { continue };
        let Some(close) = rest[open..].find('}') else {
            continue;
        };
        let parts = quoted_strings(&rest[open..open + close]);
        for pair in parts.chunks(2) {
            if let [key, value] = pair
                && key.is_empty()
            {
                out.push(normalise_root(value));
            }
        }
    }
    out
}

/// Dependencies listed by a `requirements*.txt`.
pub fn parse_requirements(src: &str) -> BTreeSet<String> {
    src.lines()
        .map(strip_comment)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('-'))
        .filter_map(requirement_name)
        .collect()
}

/// Compiled extension modules under `root`, as repo-relative paths with the
/// platform tag stripped: `pkg/_speedups.cpython-312-x86_64-linux-gnu.so`
/// becomes `pkg/_speedups.py`, which [`PyProject::place`] then names (A-10).
pub fn extension_module_paths(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        let path = entry.path();
        let is_extension = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "so" | "pyd" | "dll"));
        if !is_extension || !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let (dir, file) = match rel.rsplit_once('/') {
            Some((d, f)) => (d, f),
            None => ("", rel.as_str()),
        };
        // `_speedups.cpython-312-…so` names the module `_speedups`: PEP 3149
        // puts the ABI tag after the first dot, and a dot cannot appear in a
        // module name.
        let stem = file.split('.').next().unwrap_or(file);
        out.push(if dir.is_empty() {
            format!("{stem}.py")
        } else {
            format!("{dir}/{stem}.py")
        });
    }
    out
}

/// Directories holding an `__init__.py`, from the walked file list (A-03).
pub fn package_dirs<'a>(files: impl Iterator<Item = &'a String>) -> HashSet<String> {
    files
        .filter_map(|rel| rel.strip_suffix("__init__.py"))
        .map(|dir| dir.trim_end_matches('/').to_string())
        .collect()
}

/// The inferred root for every packaged file: the first ancestor that is not
/// itself a package (A-03).
pub fn infer_roots<'a>(
    packages: &HashSet<String>,
    files: impl Iterator<Item = &'a String>,
) -> Vec<String> {
    let probe = PyProject {
        packages: packages.clone(),
        ..PyProject::default()
    };
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for rel in files {
        let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
        if let Some(root) = probe.owning_root(dir) {
            roots.insert(root);
        }
    }
    roots.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rooted(dirs: &[&str]) -> PyProject {
        PyProject {
            packages: dirs.iter().map(|d| (*d).to_string()).collect(),
            roots: vec![String::new()],
            ..PyProject::default()
        }
    }

    #[test]
    fn a_package_init_is_the_package_itself() {
        // A-01: `__name__ == "pkg"`, not `"pkg.__init__"`.
        let cfg = rooted(&["pkg"]);
        assert_eq!(cfg.module_fqn("pkg/__init__.py"), "pkg");
        assert_eq!(cfg.module_fqn("pkg/sub.py"), "pkg.sub");
    }

    #[test]
    fn submodules_are_dotted_from_the_root_down() {
        let cfg = rooted(&["pkg", "pkg/a"]);
        assert_eq!(cfg.module_fqn("pkg/a/b.py"), "pkg.a.b");
        assert_eq!(cfg.module_fqn("pkg/a/__init__.py"), "pkg.a");
    }

    #[test]
    fn a_file_under_no_package_is_named_by_its_path() {
        // A-07/A-11: two `test_utils` are one module name at runtime and two
        // files here; the path namespace is what keeps them apart.
        let cfg = rooted(&["pkg"]);
        assert_eq!(
            cfg.module_fqn("tests/a/test_utils.py"),
            "tests/a/test_utils.py"
        );
        assert_ne!(
            cfg.module_fqn("tests/a/test_utils.py"),
            cfg.module_fqn("tests/b/test_utils.py")
        );
    }

    #[test]
    fn the_three_module_shapes_cannot_collide() {
        let single = rooted(&["pkg"]);
        let multi = PyProject {
            packages: ["src/pkg".to_string(), "pkg".to_string()].into(),
            roots: vec![String::new(), "src".to_string()],
            ..PyProject::default()
        };
        let dotted = single.module_fqn("pkg/sub.py");
        let prefixed = multi.module_fqn("src/pkg/sub.py");
        let path = single.module_fqn("scripts/run.py");
        assert_eq!(dotted, "pkg.sub");
        assert_eq!(prefixed, "src/pkg.sub");
        assert_eq!(path, "scripts/run.py");
        assert!(!dotted.contains('/'));
        assert!(!prefixed.ends_with(".py"));
        assert!(path.ends_with(".py"));
    }

    #[test]
    fn a_declared_root_beats_the_init_walk() {
        // A-05: `src/` is the root, so `src/pkg/mod.py` is `pkg.mod` — and
        // A-04's namespace package needs no `__init__.py` to say so.
        let cfg = PyProject {
            roots: vec!["src".to_string()],
            declared: true,
            ..PyProject::default()
        };
        assert_eq!(cfg.module_fqn("src/pkg/mod.py"), "pkg.mod");
        assert_eq!(cfg.module_fqn("tests/test_x.py"), "tests/test_x.py");
    }

    #[test]
    fn a_segment_that_is_not_an_identifier_falls_back_to_the_path() {
        let cfg = PyProject {
            roots: vec![String::new()],
            declared: true,
            ..PyProject::default()
        };
        assert_eq!(cfg.module_fqn("my-pkg/mod.py"), "my-pkg/mod.py");
        assert!(!is_identifier("my-pkg"));
        assert!(!is_identifier(""));
        assert!(is_identifier("_x9"));
    }

    #[test]
    fn several_roots_put_the_root_in_the_name_and_probe_own_first() {
        let cfg = PyProject {
            roots: vec![String::new(), "libs/b".to_string()],
            declared: true,
            ..PyProject::default()
        };
        assert_eq!(
            cfg.module_fqn("libs/b/common/utils.py"),
            "libs/b/common.utils"
        );
        assert_eq!(cfg.module_fqn("common/utils.py"), "./common.utils");
        // A-06: the importing file's own root is probed first, and both are
        // probed, because the same name in two distributions is two nodes.
        assert_eq!(
            cfg.module_fqns("libs/b", "common.utils"),
            ["libs/b/common.utils", "./common.utils"]
        );
    }

    #[test]
    fn one_root_leaves_the_name_plain() {
        let cfg = rooted(&["pkg"]);
        assert_eq!(cfg.module_fqns("", "pkg.sub"), ["pkg.sub"]);
    }

    #[test]
    fn setuptools_poetry_hatch_and_setup_py_all_name_src() {
        let setuptools = r#"
[tool.setuptools.packages.find]
where = ["src"]
"#;
        let poetry = r#"
[tool.poetry]
packages = [{include = "flask", from = "src"}]
"#;
        let hatch = r#"
[tool.hatch.build.targets.wheel]
packages = ["src/httpx"]
"#;
        assert_eq!(parse_pyproject(setuptools).0, ["src"]);
        assert_eq!(parse_pyproject(poetry).0, ["src"]);
        assert_eq!(parse_pyproject(hatch).0, ["src"]);
        assert_eq!(
            parse_setup_py("setup(package_dir={\"\": \"src\"})"),
            ["src"]
        );
    }

    #[test]
    fn dependencies_come_off_every_shape_a_project_states_them_in() {
        let src = r#"
[project]
name = "app"
dependencies = [
  "requests>=2.0",
  "PyYAML",
]

[project.optional-dependencies]
dev = ["pytest ; python_version > '3.8'"]
"#;
        let (_, deps) = parse_pyproject(src);
        assert!(deps.contains("requests"));
        assert!(deps.contains("pyyaml"));
        assert!(deps.contains("pytest"));

        let poetry = "[tool.poetry.dependencies]\npython = \"^3.11\"\nrequests = \"^2\"\n";
        assert_eq!(parse_pyproject(poetry).1, ["requests".to_string()].into());

        assert!(parse_requirements("# a comment\nDjango==5.2\n-r other.txt\n").contains("django"));
    }

    #[test]
    fn a_comment_marker_inside_a_requirement_url_is_not_a_comment() {
        let src = "[project]\ndependencies = [\"pkg @ https://h/a.zip#sha256=ab\"]\n";
        assert!(parse_pyproject(src).1.contains("pkg"));
    }

    #[test]
    fn roots_are_inferred_from_the_init_walk_when_nothing_declares_them() {
        let files: Vec<String> = ["pkg/__init__.py", "pkg/a/__init__.py", "pkg/a/b.py"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let packages = package_dirs(files.iter());
        assert_eq!(packages, ["pkg".to_string(), "pkg/a".to_string()].into());
        assert_eq!(infer_roots(&packages, files.iter()), [String::new()]);
    }

    #[test]
    fn the_digest_moves_with_the_manifest_and_not_with_the_walk() {
        let base = PyProject {
            roots: vec!["src".to_string()],
            declared: true,
            ..PyProject::default()
        };
        let mut same_project_more_files = base.clone();
        same_project_more_files
            .packages
            .insert("src/pkg/new".to_string());
        assert_eq!(base.digest(), same_project_more_files.digest());

        let mut different_layout = base.clone();
        different_layout.roots = vec![String::new()];
        assert_ne!(base.digest(), different_layout.digest());
    }
}
