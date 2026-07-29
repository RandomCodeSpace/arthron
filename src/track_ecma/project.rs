//! Project layout: every `package.json` scope and every `tsconfig.json` in the
//! repository, read once per scan.
//!
//! This is the subsystem Go does not have. A Go module is one `go.mod` at the
//! root and a package path derived from a directory name; EcmaScript
//! resolution is *configuration-directed* and the configuration is a graph —
//! `extends` chains, nested package scopes, workspace members. NODE
//! `LOOKUP_PACKAGE_SCOPE` walks up from the importing file, so "which
//! manifest governs this file" has as many answers as there are directories.
//!
//! Nothing here reads a source file. Manifests are resolver inputs, exactly as
//! `go.mod` is: the extractor still sees one file and no configuration.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::model::NodeId;
use crate::track_ecma::json::{Json, parse};
use crate::track_ecma::lang::ModuleKind;

/// Directory names the layout walk never descends into.
///
/// The same list the two [`crate::lang::Language`] impls skip, for the same
/// reason: a manifest inside `node_modules` describes a dependency, and a
/// manifest inside `dist` describes build output. Reading either would let
/// code this scan does not index decide the identity of code it does.
pub const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    "out",
    "coverage",
    ".next",
    ".nuxt",
];

/// Node's built-in modules. NODE: the authoritative list is
/// `module.builtinModules`; this is that list, vendored.
///
/// A list and not a heuristic. Go's "no dot in the first path segment" trick
/// does not transfer — `lodash` has no dot either — and guessing here is how a
/// dependency silently becomes an unresolved reference.
pub const NODE_BUILTINS: &[&str] = &[
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// File extensions that are data or assets rather than code.
///
/// A16/A15: a bundler lets `import './logo.svg'` mean "the URL of this asset".
/// It is a real dependency on something outside the graph's vocabulary, so it
/// is `External`, not a resolution failure — reporting `ModuleNotFound` for
/// every stylesheet in a frontend corpus would bury the failures that matter.
pub const ASSET_EXTENSIONS: &[&str] = &[
    "json", "css", "scss", "sass", "less", "svg", "png", "jpg", "jpeg", "gif", "webp", "avif",
    "woff", "woff2", "ttf", "eot", "wasm", "txt", "md", "html", "yml", "yaml", "toml", "graphql",
    "gql", "node",
];

/// Code extensions in the EcmaScript family that **no** [`crate::model::Lang`]
/// owns in this build.
///
/// `Lang::JavaScript` owns `js`/`mjs`/`cjs` and `Lang::TypeScript` owns `ts`,
/// so a specifier reaching a `.tsx` file reaches real code that this build
/// never indexed. That is [`crate::UnresolvedReason::TierTwoLanguage`] — the
/// language is parsed structurally elsewhere but not resolved here — and
/// calling it `ModuleNotFound` would blame the code for a gap in the tool.
pub const UNOWNED_CODE_EXTENSIONS: &[&str] = &["tsx", "jsx", "mts", "cts"];

/// One `package.json`'s contribution to resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageScope {
    /// Repo-relative directory holding the manifest. `""` at the root.
    pub dir: String,
    /// The `"name"` field — what a self-reference or a workspace import names.
    pub name: Option<String>,
    /// The `"type"` field, as a module kind. `None` when unstated.
    pub module_type: Option<ModuleKind>,
    /// `"main"`, the CommonJS entry point.
    pub main: Option<String>,
    /// `"module"`, the bundler-era ESM entry point. **[non-spec]**, but
    /// universal enough that ignoring it turns most workspace imports into
    /// misses.
    pub module_entry: Option<String>,
    /// `"types"`/`"typings"`, the declaration entry point.
    pub types: Option<String>,
    /// The `"exports"` map, unparsed. NODE `PACKAGE_EXPORTS_RESOLVE` needs its
    /// key order, so it stays a [`Json`] rather than becoming a map.
    pub exports: Option<Json>,
    /// The `"imports"` map, for `#`-prefixed specifiers.
    pub imports: Option<Json>,
    /// Every declared dependency name, across `dependencies`,
    /// `devDependencies`, `peerDependencies` and `optionalDependencies`.
    pub dependencies: BTreeSet<String>,
}

/// One `tsconfig.json`'s contribution, with its `extends` chain flattened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TsProject {
    /// Repo-relative directory holding the config.
    pub dir: String,
    /// `compilerOptions.baseUrl`, made repo-relative. `None` when unstated.
    pub base_url: Option<String>,
    /// `compilerOptions.paths`, each pattern with its substitutions made
    /// repo-relative, in declaration order.
    pub paths: Vec<(String, Vec<String>)>,
    /// `compilerOptions.customConditions`: conditions this project adds to
    /// the set NODE `PACKAGE_TARGET_RESOLVE` is given, on top of the ones the
    /// module kind and dialect decide.
    ///
    /// A monorepo that publishes built artefacts uses one of these to point
    /// its own compilation at the sources instead — `"@zod/source"` ahead of
    /// `"types"` in the same `exports` entry. Ignoring it makes every
    /// intra-repository import take the published branch, which names a file
    /// no scan of the sources can see, and every name reached through that
    /// import misses with it.
    ///
    /// Order is recorded but does not decide anything: NODE matches
    /// conditions in the *map's* key order and consults this only for
    /// membership.
    pub custom_conditions: Vec<String>,
    /// `compilerOptions.types`: the ambient type packages this project puts
    /// in scope without an import.
    ///
    /// Read for [`EcmaConfig::declares_ambient`] and nothing else. Distinct
    /// from [`PackageScope::types`], which is one package's declaration entry
    /// point.
    pub ambient_types: Vec<String>,
}

/// Everything the EcmaScript resolver learned about the project's layout.
///
/// Built once per scan by [`build`], from manifests only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EcmaConfig {
    /// The repository root. Absolute, machine-local, and deliberately **not**
    /// part of [`EcmaConfig::digest`]: a store built under `/home/a` describes
    /// the same project as one built under `/home/b`.
    pub root: PathBuf,
    /// Every package scope, deepest directory first, so the first match found
    /// walking the list is NODE `LOOKUP_PACKAGE_SCOPE`'s answer.
    pub scopes: Vec<PackageScope>,
    /// Every TypeScript project, deepest directory first.
    pub ts_projects: Vec<TsProject>,
    /// A fingerprint of every manifest fact above.
    pub digest: Vec<u8>,
    /// Every module the store holds, by identity, so a star re-export's
    /// target can be re-entered by path.
    ///
    /// Learned from the store between the two phases, never read from a
    /// manifest, and so deliberately outside [`EcmaConfig::digest`]: it
    /// changes as the scan learns rather than as the project does. A cold
    /// scan fills it from what phase 1 has just written, which is why
    /// following a star works on the first run and not only the second.
    pub module_paths: HashMap<NodeId, String>,
}

impl EcmaConfig {
    /// The nearest enclosing package scope of a repo-relative file, NODE
    /// `LOOKUP_PACKAGE_SCOPE`.
    pub fn scope_for(&self, rel_path: &str) -> Option<&PackageScope> {
        let dir = parent_dir(rel_path);
        self.scopes.iter().find(|s| dir_contains(&s.dir, dir))
    }

    /// The nearest enclosing TypeScript project of a repo-relative file.
    ///
    /// "Nearest" rather than "the one whose `include` covers it": overlapping
    /// `include` globs are legal and ambiguous (§12.4 of the case study), and
    /// nearest-wins is a rule a reader can check by looking at the tree.
    pub fn ts_for(&self, rel_path: &str) -> Option<&TsProject> {
        let dir = parent_dir(rel_path);
        self.ts_projects.iter().find(|p| dir_contains(&p.dir, dir))
    }

    /// The module kind the nearest `package.json` `"type"` decides for a file
    /// whose own text did not say. NODE `ESM_FILE_FORMAT`: the default is
    /// CommonJS.
    pub fn module_kind_for(&self, rel_path: &str) -> ModuleKind {
        self.scope_for(rel_path)
            .and_then(|s| s.module_type)
            .unwrap_or(ModuleKind::CommonJs)
    }

    /// Whether a bare specifier's package is a declared dependency of the
    /// nearest scope or of any scope above it.
    pub fn is_declared_dependency(&self, rel_path: &str, package: &str) -> bool {
        let dir = parent_dir(rel_path);
        self.scopes
            .iter()
            .filter(|s| dir_contains(&s.dir, dir))
            .any(|s| s.dependencies.contains(package))
    }

    /// Whether an ambient environment's package is present for a file.
    ///
    /// Two channels, because a project states the fact in two places and
    /// either one is the project saying it. `package.json` is the general
    /// one; `tsconfig.json`'s `compilerOptions.types` is TypeScript's own,
    /// and it is what a vendored workspace member has when its manifest
    /// declares no dependencies at all.
    ///
    /// Deliberately not a check that the package is *installed*: nothing under
    /// `node_modules` is indexed, and a manifest that names a dependency is
    /// the strongest statement about the project this scan can read.
    pub fn declares_ambient(&self, rel_path: &str, package: &str) -> bool {
        if self.is_declared_dependency(rel_path, package) {
            return true;
        }
        // TypeScript writes `@types/mocha` into `types` as `mocha`, so the
        // bare name is compared against both spellings the table can carry.
        let bare = package.strip_prefix("@types/").unwrap_or(package);
        self.ts_for(rel_path)
            .is_some_and(|p| p.ambient_types.iter().any(|t| t == package || t == bare))
    }

    /// The workspace package a bare specifier's package name refers to, when
    /// one in this repository declares that `"name"`.
    pub fn workspace_package(&self, package: &str) -> Option<&PackageScope> {
        self.scopes
            .iter()
            .find(|s| s.name.as_deref() == Some(package))
    }
}

/// Whether `dir` is `candidate` or an ancestor of it, on `/` boundaries.
fn dir_contains(candidate: &str, dir: &str) -> bool {
    if candidate.is_empty() {
        return true; // the root scope contains everything
    }
    if dir == candidate {
        return true;
    }
    dir.len() > candidate.len()
        && dir.starts_with(candidate)
        && dir.as_bytes()[candidate.len()] == b'/'
}

/// The directory part of a repo-relative path, `""` at the root.
pub fn parent_dir(rel_path: &str) -> &str {
    match rel_path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Join a repo-relative directory with a possibly-relative path and normalise
/// `.`/`..`, producing a repo-relative `/`-separated path.
///
/// `None` when the result escapes the repository root: a specifier reaching
/// outside the tree names something this scan cannot own, and inventing a
/// clamped path for it would silently point the reference at the wrong file.
pub fn join_normalized(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if !rel.starts_with('/') && !dir.is_empty() {
        parts.extend(dir.split('/').filter(|p| !p.is_empty()));
    }
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Read every manifest under `root` and fold it into one configuration.
///
/// Never fails: a repository with no `package.json` at all is a legitimate
/// pile of scripts, and the resolver's answer for it — relative specifiers
/// resolve, bare ones are unknown packages — is honest rather than an abort.
pub fn build(root: &Path) -> EcmaConfig {
    let mut scopes: Vec<PackageScope> = Vec::new();
    let mut ts_sources: BTreeMap<String, Json> = BTreeMap::new();

    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(rel_os) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel_os.to_string_lossy().replace('\\', "/");
        if rel.split('/').any(|c| SKIP_DIRS.contains(&c)) {
            continue;
        }
        let name = match rel.rsplit_once('/') {
            Some((_, file)) => file,
            None => rel.as_str(),
        };
        let is_tsconfig =
            name.starts_with("tsconfig") && name.ends_with(".json") || name == "jsconfig.json";
        if name != "package.json" && !is_tsconfig {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Some(value) = parse(&text) else {
            continue; // a manifest that does not parse states nothing
        };
        if name == "package.json" {
            scopes.push(package_scope(parent_dir(&rel).to_string(), &value));
        } else {
            ts_sources.insert(rel, value);
        }
    }

    // Deepest first, so the first hit walking the list is the nearest scope.
    scopes.sort_by(|a, b| depth_order(&a.dir, &b.dir));

    let mut ts_projects: Vec<TsProject> = ts_sources
        .keys()
        .filter(|rel| {
            // Only the config a directory would actually be governed by. A
            // `tsconfig.build.json` beside `tsconfig.json` describes an
            // emit, not the source layout.
            matches!(
                rel.rsplit_once('/').map_or(rel.as_str(), |(_, f)| f),
                "tsconfig.json" | "jsconfig.json"
            )
        })
        .map(|rel| ts_project(rel, &ts_sources, root))
        .collect();
    ts_projects.sort_by(|a, b| depth_order(&a.dir, &b.dir));

    let digest = digest_of(&scopes, &ts_projects);
    EcmaConfig {
        root: root.to_path_buf(),
        scopes,
        ts_projects,
        digest,
        module_paths: HashMap::new(),
    }
}

/// Deeper directories first; ties broken by name so the order is total.
fn depth_order(a: &str, b: &str) -> std::cmp::Ordering {
    let depth = |d: &str| {
        if d.is_empty() {
            0
        } else {
            d.split('/').count()
        }
    };
    depth(b).cmp(&depth(a)).then_with(|| a.cmp(b))
}

fn package_scope(dir: String, value: &Json) -> PackageScope {
    let text = |key: &str| value.get(key).and_then(Json::as_str).map(str::to_string);
    let mut dependencies = BTreeSet::new();
    for field in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(map) = value.get(field) {
            for (name, _) in map.entries() {
                dependencies.insert(name.to_string());
            }
        }
    }
    PackageScope {
        dir,
        name: text("name"),
        module_type: match value.get("type").and_then(Json::as_str) {
            Some("module") => Some(ModuleKind::Esm),
            Some("commonjs") => Some(ModuleKind::CommonJs),
            _ => None,
        },
        main: text("main"),
        module_entry: text("module"),
        types: text("types").or_else(|| text("typings")),
        exports: value.get("exports").cloned(),
        imports: value.get("imports").cloned(),
        dependencies,
    }
}

/// Flatten one tsconfig's `extends` chain into `baseUrl` + `paths`.
///
/// The chain is bounded: `extends` may name a file that names another, and a
/// cycle in a hand-written config is a typo rather than a reason to hang.
fn ts_project(rel: &str, sources: &BTreeMap<String, Json>, root: &Path) -> TsProject {
    const MAX_EXTENDS: usize = 16;
    let dir = parent_dir(rel).to_string();
    let mut base_url: Option<String> = None;
    let mut paths: Vec<(String, Vec<String>)> = Vec::new();
    let mut custom_conditions: Option<Vec<String>> = None;
    let mut ambient_types: Option<Vec<String>> = None;

    // Nearest config first: a value set by the child wins over the base it
    // extends, which is what `extends` means.
    let mut current: Option<(String, Json)> =
        sources.get(rel).map(|v| (rel.to_string(), v.clone()));
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut hops = 0;
    while let Some((at, value)) = current.take() {
        if !seen.insert(at.clone()) || hops >= MAX_EXTENDS {
            break;
        }
        hops += 1;
        let at_dir = parent_dir(&at).to_string();
        let options = value.get("compilerOptions");
        if base_url.is_none()
            && let Some(raw) = options
                .and_then(|o| o.get("baseUrl"))
                .and_then(Json::as_str)
        {
            base_url = join_normalized(&at_dir, raw);
        }
        if paths.is_empty()
            && let Some(map) = options.and_then(|o| o.get("paths"))
        {
            // TS ≥ 4.1: with no `baseUrl`, substitutions resolve against the
            // directory of the config that declared them — not the child's.
            let anchor = base_url.clone().unwrap_or(at_dir.clone());
            for (pattern, targets) in map.entries() {
                let subs: Vec<String> = targets
                    .items()
                    .iter()
                    .filter_map(Json::as_str)
                    .filter_map(|t| join_normalized(&anchor, t))
                    .collect();
                if !subs.is_empty() {
                    paths.push((pattern.to_string(), subs));
                }
            }
        }
        // Nearest-wins, exactly as `paths` above: a child that states either
        // option replaces the base's rather than adding to it, which is what
        // `extends` means for a `compilerOptions` key.
        //
        // *Stated*, not *non-empty*, and that is why these two are `Option`
        // where `paths` above is a `Vec`. `"types": []` is the documented way
        // to say "no ambient type packages at all" and `"customConditions":
        // []` says the same about conditions; reading an empty list as
        // "unstated" would hand the child back the base's value it had just
        // switched off. `paths` can use emptiness as the proxy because an
        // empty map substitutes nothing either way, and these two decide a
        // lookup: an inherited `"types": ["jest"]` turns an ambient
        // environment on under a child that turned it off, and an inherited
        // condition sends an import down a branch tsc would not take.
        if custom_conditions.is_none()
            && let Some(list) = options.and_then(|o| o.get("customConditions"))
        {
            custom_conditions = Some(strings(list));
        }
        if ambient_types.is_none()
            && let Some(list) = options.and_then(|o| o.get("types"))
        {
            ambient_types = Some(strings(list));
        }
        let Some(extends) = value.get("extends").and_then(Json::as_str) else {
            break;
        };
        // Only a relative `extends` names a file in this repository; a package
        // specifier names one under `node_modules`, which is not indexed.
        if !extends.starts_with('.') {
            break;
        }
        let Some(target) = join_normalized(&at_dir, extends) else {
            break;
        };
        let target = if target.ends_with(".json") {
            target
        } else {
            format!("{target}.json")
        };
        current = match sources.get(&target) {
            Some(v) => Some((target, v.clone())),
            // Outside the walk (skipped directory, or genuinely absent): read
            // it directly rather than silently dropping the base's `paths`.
            None => std::fs::read_to_string(root.join(&target))
                .ok()
                .and_then(|t| parse(&t))
                .map(|v| (target, v)),
        };
    }

    TsProject {
        dir,
        base_url,
        paths,
        custom_conditions: custom_conditions.unwrap_or_default(),
        ambient_types: ambient_types.unwrap_or_default(),
    }
}

/// A `compilerOptions` array of strings, with anything else in it dropped.
fn strings(list: &Json) -> Vec<String> {
    list.items()
        .iter()
        .filter_map(Json::as_str)
        .map(str::to_string)
        .collect()
}

/// A fingerprint of everything the manifests decide.
///
/// Every field is length-prefixed so no pair of values can be concatenated
/// into another. The repository root is absent on purpose: it is where the
/// project sits, not what it is, and folding it in would wipe the store for
/// every checkout of the same tree.
fn digest_of(scopes: &[PackageScope], ts: &[TsProject]) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    let mut field = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(&(scopes.len() as u64).to_le_bytes());
    for s in scopes {
        field(s.dir.as_bytes());
        field(s.name.as_deref().unwrap_or("").as_bytes());
        field(match s.module_type {
            Some(ModuleKind::Esm) => b"esm",
            Some(ModuleKind::CommonJs) => b"cjs",
            _ => b"",
        });
        field(s.main.as_deref().unwrap_or("").as_bytes());
        field(s.module_entry.as_deref().unwrap_or("").as_bytes());
        field(s.types.as_deref().unwrap_or("").as_bytes());
        field(format!("{:?}", s.exports).as_bytes());
        field(format!("{:?}", s.imports).as_bytes());
        field(&(s.dependencies.len() as u64).to_le_bytes());
        for d in &s.dependencies {
            field(d.as_bytes());
        }
    }
    field(&(ts.len() as u64).to_le_bytes());
    for p in ts {
        field(p.dir.as_bytes());
        field(p.base_url.as_deref().unwrap_or("").as_bytes());
        field(&(p.paths.len() as u64).to_le_bytes());
        for (pattern, subs) in &p.paths {
            field(pattern.as_bytes());
            for sub in subs {
                field(sub.as_bytes());
            }
        }
        // Both decide resolution, so both belong in the fence: editing a
        // `customConditions` entry re-points every conditional import in the
        // project, and a store built before the edit would answer with the
        // old targets until an unrelated file happened to change.
        field(&(p.custom_conditions.len() as u64).to_le_bytes());
        for c in &p.custom_conditions {
            field(c.as_bytes());
        }
        field(&(p.ambient_types.len() as u64).to_le_bytes());
        for t in &p.ambient_types {
            field(t.as_bytes());
        }
    }
    hasher.finalize().as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn joining_normalises_and_refuses_to_escape_the_root() {
        assert_eq!(join_normalized("src", "./util"), Some("src/util".into()));
        assert_eq!(join_normalized("src/a", "../b/c"), Some("src/b/c".into()));
        assert_eq!(join_normalized("", "./x/./y"), Some("x/y".into()));
        assert_eq!(join_normalized("src", "/abs/x"), Some("abs/x".into()));
        // Escaping the tree is `None`, never a clamp: a clamped path names a
        // different file, and naming the wrong file is worse than a miss.
        assert_eq!(join_normalized("src", "../../outside"), None);
    }

    #[test]
    fn the_nearest_scope_wins_and_decides_the_module_kind() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", r#"{"name":"app","type":"commonjs"}"#);
        write(
            root,
            "packages/ui/package.json",
            r#"{"name":"@app/ui","type":"module"}"#,
        );
        write(root, "packages/ui/src/index.js", "");
        write(root, "lib/util.js", "");

        let cfg = build(root);
        assert_eq!(
            cfg.scope_for("packages/ui/src/index.js")
                .map(|s| s.dir.as_str()),
            Some("packages/ui")
        );
        assert_eq!(
            cfg.scope_for("lib/util.js").map(|s| s.dir.as_str()),
            Some("")
        );
        // NODE `ESM_FILE_FORMAT`: `"type"` of the nearest enclosing scope.
        assert_eq!(
            cfg.module_kind_for("packages/ui/src/index.js"),
            ModuleKind::Esm
        );
        assert_eq!(cfg.module_kind_for("lib/util.js"), ModuleKind::CommonJs);
        assert_eq!(
            cfg.workspace_package("@app/ui").map(|s| s.dir.as_str()),
            Some("packages/ui")
        );
    }

    #[test]
    fn a_missing_manifest_is_commonjs_and_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.js", "");
        let cfg = build(dir.path());
        assert!(cfg.scopes.is_empty());
        assert_eq!(cfg.module_kind_for("a.js"), ModuleKind::CommonJs);
    }

    #[test]
    fn tsconfig_paths_flatten_through_extends() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "tsconfig.base.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@vue/*":["packages/*/src"]}}}"#,
        );
        write(
            root,
            "tsconfig.json",
            r#"{"extends":"./tsconfig.base.json"}"#,
        );
        write(root, "packages/reactivity/src/index.ts", "");

        let cfg = build(root);
        let ts = cfg
            .ts_for("packages/reactivity/src/index.ts")
            .expect("a project");
        assert_eq!(ts.base_url.as_deref(), Some(""));
        assert_eq!(
            ts.paths,
            vec![("@vue/*".to_string(), vec!["packages/*/src".to_string()])]
        );
    }

    #[test]
    fn skipped_directories_contribute_no_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", r#"{"name":"app"}"#);
        write(root, "node_modules/dep/package.json", r#"{"name":"dep"}"#);
        write(root, "dist/package.json", r#"{"name":"built"}"#);
        let cfg = build(root);
        let dirs: Vec<&str> = cfg.scopes.iter().map(|s| s.dir.as_str()).collect();
        assert_eq!(dirs, [""]);
        assert_eq!(cfg.workspace_package("dep"), None);
    }

    #[test]
    fn the_digest_covers_manifest_facts_and_not_the_checkout_path() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write(
            a.path(),
            "package.json",
            r#"{"name":"app","type":"module"}"#,
        );
        write(
            b.path(),
            "package.json",
            r#"{"name":"app","type":"module"}"#,
        );
        assert_eq!(build(a.path()).digest, build(b.path()).digest);

        let c = tempfile::tempdir().unwrap();
        write(
            c.path(),
            "package.json",
            r#"{"name":"app","type":"commonjs"}"#,
        );
        assert_ne!(build(a.path()).digest, build(c.path()).digest);
    }

    #[test]
    fn dependencies_come_from_every_declaration_field() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"dependencies":{"a":"1"},"devDependencies":{"b":"2"},"peerDependencies":{"c":"3"}}"#,
        );
        let cfg = build(dir.path());
        for name in ["a", "b", "c"] {
            assert!(cfg.is_declared_dependency("src/x.js", name), "{name}");
        }
        assert!(!cfg.is_declared_dependency("src/x.js", "d"));
    }
}
