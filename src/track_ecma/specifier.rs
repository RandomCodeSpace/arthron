//! Specifier → module. The half of the EcmaScript core that JavaScript and
//! TypeScript genuinely share, because it is *Node's* algorithm rather than
//! either language's: NODE `ESM_RESOLVE` and `require`, with `exports`,
//! `imports`, conditions and the package boundary. TypeScript layers extra
//! *inputs* on the same algorithm — `paths`, `baseUrl`, extension
//! substitution — and so does JavaScript tooling, which is why this is one
//! resolver parameterised by a configuration and not two resolvers.
//!
//! Everything here produces **ordered candidate module paths**, never an
//! answer. Probing is the caller's, so that every probe — hit or miss — lands
//! in the candidate-set invalidation index. A3 is the strongest confirmation
//! of that design found anywhere in the case studies: adding `src/util.js`
//! beside `src/util/index.js` must re-point every importer, and it does,
//! because the miss on `src/util.js` was recorded the first time.

use crate::UnresolvedReason;
use crate::track_ecma::json::Json;
use crate::track_ecma::lang::{Dialect, ModuleKind};
use crate::track_ecma::project::{
    ASSET_EXTENSIONS, EcmaConfig, NODE_BUILTINS, PackageScope, SKIP_DIRS, join_normalized,
    parent_dir,
};

/// What a specifier names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    /// Repo-relative module paths to probe, in order. First hit wins.
    ///
    /// `fallback` is what the specifier names when **every** probe misses: a
    /// configured alias and a `baseUrl` are a compile-time overlay on top of
    /// the `node_modules` walk, not a replacement for it, so `lodash` under a
    /// `baseUrl` must still be the declared dependency rather than a missing
    /// file. `None` means a miss is a miss.
    Candidates {
        /// Module paths to probe, in order.
        paths: Vec<String>,
        /// The external key to fall back to when none of them exists.
        fallback: Option<String>,
    },
    /// Something outside the repository, by its external key.
    External(String),
    /// Nothing this build can name, with the reason.
    Unresolved(UnresolvedReason),
}

/// The condition set an importer supplies to NODE `PACKAGE_TARGET_RESOLVE`.
///
/// A9: the set depends on the *importing* file, so one target package can
/// resolve to two different files from two importers in one repository. The
/// `"types"` condition leads TypeScript's, per the documented requirement that
/// it precede `"import"`/`"require"`.
fn conditions(kind: ModuleKind, dialect: Dialect) -> &'static [&'static str] {
    match (dialect, kind) {
        (Dialect::TypeScript, ModuleKind::CommonJs) => &["types", "node", "require", "default"],
        (Dialect::TypeScript, _) => &["types", "node", "import", "default"],
        (Dialect::JavaScript, ModuleKind::CommonJs) => &["node", "require", "default"],
        (Dialect::JavaScript, _) => &["node", "import", "default"],
    }
}

/// Resolve one module specifier from one importing file.
pub fn resolve(
    cfg: &EcmaConfig,
    importer: &str,
    spec: &str,
    kind: ModuleKind,
    dialect: Dialect,
) -> Spec {
    // A16: a bundler query suffix is not part of the identity. Node would
    // fail on it; not stripping it is a permanent miss on every asset import
    // in a modern frontend corpus.
    let spec = spec.split('?').next().unwrap_or(spec);
    if spec.is_empty() {
        return Spec::Unresolved(UnresolvedReason::ModuleNotFound);
    }

    // A27: a URL specifier names a host this build is not a host for. It is a
    // declared boundary, not a failure — and inventing a reason for it would
    // put a design decision in the unresolved denominator.
    for scheme in ["http://", "https://", "data:", "file:", "blob:"] {
        if spec.starts_with(scheme) {
            return Spec::External(format!("url:{}", scheme.trim_end_matches(['/', ':'])));
        }
    }
    // A13: the `node:` scheme always names a builtin; some builtins are
    // reachable *only* through it.
    if let Some(rest) = spec.strip_prefix("node:") {
        return Spec::External(format!("node:{rest}"));
    }
    if NODE_BUILTINS.contains(&spec) {
        return Spec::External(format!("node:{spec}"));
    }

    // A11: `#`-prefixed specifiers resolve **only** against the nearest
    // package scope of the importing file, never against the root and never
    // inherited from a dependency.
    if spec.starts_with('#') {
        let Some(scope) = cfg.scope_for(importer) else {
            return Spec::Unresolved(UnresolvedReason::ModuleNotFound);
        };
        let Some(map) = &scope.imports else {
            return Spec::Unresolved(UnresolvedReason::ModuleNotFound);
        };
        return match subpath_resolve(map, spec, conditions(kind, dialect)) {
            Target::Path(target) => from_package_relative(cfg, scope, &target, kind, dialect),
            Target::Blocked => Spec::Unresolved(UnresolvedReason::NotExported),
            Target::None => Spec::Unresolved(UnresolvedReason::ModuleNotFound),
        };
    }

    // NODE: `./`, `../` and `/` are relative, and so are the bare `.` and
    // `..` that `require('..')` uses to name the package root — a form real
    // CommonJS test suites lean on heavily. Missing them sends a relative
    // specifier down the bare-specifier path and reports `UnknownPackage` for
    // a directory that is right there.
    if spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/')
        || spec == "."
        || spec == ".."
    {
        let Some(base) = join_normalized(parent_dir(importer), spec) else {
            return Spec::Unresolved(UnresolvedReason::ModuleNotFound);
        };
        return from_base(cfg, &base, kind, dialect);
    }

    bare(cfg, importer, spec, kind, dialect)
}

/// A bare specifier: an alias, a workspace member, a dependency, or nothing.
fn bare(cfg: &EcmaConfig, importer: &str, spec: &str, kind: ModuleKind, dialect: Dialect) -> Spec {
    let mut candidates: Vec<String> = Vec::new();
    let mut fallback: Option<String> = None;

    // A14/A10: configured aliases first. Reading the config is the whole
    // difference between resolving a modern frontend and guessing that `@/`
    // means `src/` — which is the predecessor's failure mode in miniature.
    if let Some(project) = cfg.ts_for(importer) {
        for base in paths_substitutions(project.paths.as_slice(), spec) {
            extend(&mut candidates, probe_list(cfg, &base, kind, dialect));
        }
    }

    let (package, subpath) = split_package(spec);
    // A12/A7: a workspace member is repo-internal even though a bare
    // specifier names it, and skipping this turns every intra-monorepo edge
    // into `External`.
    if let Some(scope) = cfg.workspace_package(package) {
        match package_entry(scope, subpath, kind, dialect) {
            Target::Path(target) => match from_package_relative(cfg, scope, &target, kind, dialect)
            {
                Spec::Candidates { paths, .. } => extend(&mut candidates, paths),
                // A workspace entry point that is an asset — a package whose
                // `"main"` is a `.json` — is that asset, not a miss.
                Spec::External(key) => fallback = Some(key),
                Spec::Unresolved(_) => {}
            },
            // A8: `"exports"` is exhaustive. A deep import of a path the map
            // does not list fails even though the file exists, and saying
            // `NoMatchingDefinition` would send the reader to the wrong file.
            Target::Blocked => return Spec::Unresolved(UnresolvedReason::NotExported),
            Target::None => {}
        }
    }

    // A9 (tsconfig): `baseUrl` is tried after the package walk, not before.
    if let Some(project) = cfg.ts_for(importer)
        && let Some(base_url) = &project.base_url
        && let Some(base) = join_normalized(base_url, spec)
    {
        extend(&mut candidates, probe_list(cfg, &base, kind, dialect));
    }

    // A7: `node_modules` is the dependency boundary and is not indexed, so a
    // declared dependency is `External` exactly as a Go `require` is. It is a
    // *fallback* and not an answer, because a `paths` alias or a workspace
    // member of the same name is resolved first.
    if fallback.is_none() && cfg.is_declared_dependency(importer, package) {
        fallback = Some(format!("npm:{package}"));
    }
    if !candidates.is_empty() {
        return Spec::Candidates {
            paths: candidates,
            fallback,
        };
    }
    if let Some(key) = fallback {
        return Spec::External(key);
    }
    // Not a builtin, not configured, not declared. `UnknownPackage` is the
    // honest answer and it is the same one Go gives; A14's `UnconfiguredAlias`
    // is deliberately folded into it rather than splitting the histogram over
    // a distinction nothing yet acts on.
    Spec::Unresolved(UnresolvedReason::UnknownPackage)
}

/// `@scope/name/sub/path` → `("@scope/name", "./sub/path")`.
fn split_package(spec: &str) -> (&str, String) {
    let parts: Vec<&str> = spec
        .splitn(if spec.starts_with('@') { 3 } else { 2 }, '/')
        .collect();
    let take = if spec.starts_with('@') { 2 } else { 1 };
    if parts.len() <= take {
        return (spec, ".".to_string());
    }
    let package_len: usize = parts[..take].iter().map(|p| p.len()).sum::<usize>() + take - 1;
    (&spec[..package_len], format!("./{}", parts[take]))
}

/// A10: substitute a bare specifier through tsconfig `paths`.
///
/// The pattern holds at most one `*`; the pattern with the **longest matching
/// prefix** wins, and its substitutions are tried in array order. Anything
/// else — first-match-in-file-order — resolves `@app/x` through `@app/*` when
/// a more specific `@app/x/*` was written for it.
fn paths_substitutions(paths: &[(String, Vec<String>)], spec: &str) -> Vec<String> {
    let mut best: Option<(usize, &Vec<String>, String)> = None;
    for (pattern, subs) in paths {
        let Some((prefix, suffix)) = pattern.split_once('*') else {
            if pattern == spec {
                // An exact pattern beats every wildcard: its prefix is the
                // whole specifier, which is the longest one possible.
                best = Some((usize::MAX, subs, String::new()));
            }
            continue;
        };
        if !spec.starts_with(prefix) || !spec.ends_with(suffix) {
            continue;
        }
        if spec.len() < prefix.len() + suffix.len() {
            continue;
        }
        let matched = spec[prefix.len()..spec.len() - suffix.len()].to_string();
        if best.as_ref().is_none_or(|(len, _, _)| prefix.len() > *len) {
            best = Some((prefix.len(), subs, matched));
        }
    }
    let Some((_, subs, matched)) = best else {
        return Vec::new();
    };
    subs.iter().map(|s| s.replace('*', &matched)).collect()
}

/// A target string a `package.json` field produced, resolved against the
/// package's own directory.
fn from_package_relative(
    cfg: &EcmaConfig,
    scope: &PackageScope,
    target: &str,
    kind: ModuleKind,
    dialect: Dialect,
) -> Spec {
    match join_normalized(&scope.dir, target) {
        Some(base) => from_base(cfg, &base, kind, dialect),
        None => Spec::Unresolved(UnresolvedReason::ModuleNotFound),
    }
}

/// A resolved base path: assets short-circuit, everything else becomes probes.
fn from_base(cfg: &EcmaConfig, base: &str, kind: ModuleKind, dialect: Dialect) -> Spec {
    if let Some(ext) = asset_extension(base) {
        return Spec::External(format!("asset:{ext}"));
    }
    let candidates = probe_list(cfg, base, kind, dialect);
    if candidates.is_empty() {
        return Spec::Unresolved(UnresolvedReason::ModuleNotFound);
    }
    Spec::Candidates {
        paths: candidates,
        fallback: None,
    }
}

/// The extension of a path's last segment, lowercased, when it has one.
fn extension(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next()?;
    let (_, ext) = last.rsplit_once('.')?;
    (!ext.is_empty()).then_some(ext)
}

/// The asset extension of a path, when the path names data rather than code.
pub fn asset_extension(path: &str) -> Option<&str> {
    extension(path).filter(|ext| ASSET_EXTENSIONS.contains(ext))
}

/// Whether a repo-relative path sits inside a directory the scan skips.
///
/// Such a path can never be a module node, so probing it would add a
/// permanent miss to the invalidation index for an identity nothing will ever
/// declare.
fn skipped(path: &str) -> bool {
    path.split('/').any(|c| SKIP_DIRS.contains(&c))
}

fn extend(into: &mut Vec<String>, more: Vec<String>) {
    for item in more {
        if !into.contains(&item) {
            into.push(item);
        }
    }
}

/// The ordered file probes for one resolved base path.
///
/// **Module-kind dependent, and non-negotiably so** (A5): ESM performs URL
/// resolution only — file extensions are mandatory for the `import` keyword
/// and a directory import throws — while CommonJS probes `X`, `X.js`,
/// `X.json`, `X.node` and then the directory. Applying one list to both is
/// wrong in one direction or the other for every relative specifier in the
/// corpus. TypeScript's `moduleResolution` probes regardless, plus the
/// output-extension rewrite table (A3).
fn probe_list(cfg: &EcmaConfig, base: &str, kind: ModuleKind, dialect: Dialect) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: String| {
        if !p.is_empty() && !skipped(&p) && !out.contains(&p) {
            out.push(p);
        }
    };
    if base.is_empty() {
        // The repository root itself — `require('..')` from a test directory,
        // and a package self-reference with no `"exports"`. It is a directory,
        // so only `LOAD_AS_DIRECTORY` applies.
        directory_probes(cfg, base, kind, dialect, &mut push);
        return out;
    }

    if base.ends_with(".d.ts") {
        push(base.to_string());
        return out;
    }
    match (extension(base), dialect) {
        // A3: under `node16`/`nodenext` a relative ESM specifier carries the
        // *output* extension, so `./x.js` names `x.ts` on disk.
        (Some("js"), Dialect::TypeScript) => {
            let stem = &base[..base.len() - 2];
            for suffix in ["ts", "tsx", "d.ts", "js", "jsx"] {
                push(format!("{stem}{suffix}"));
            }
        }
        (Some("mjs"), Dialect::TypeScript) => {
            let stem = &base[..base.len() - 3];
            push(format!("{stem}mts"));
            push(format!("{stem}mjs"));
        }
        (Some("cjs"), Dialect::TypeScript) => {
            let stem = &base[..base.len() - 3];
            push(format!("{stem}cts"));
            push(format!("{stem}cjs"));
        }
        (Some("jsx"), Dialect::TypeScript) => {
            let stem = &base[..base.len() - 3];
            push(format!("{stem}tsx"));
            push(format!("{stem}jsx"));
        }
        // An extension this build recognises as code: take it literally.
        (Some(ext), _) if is_code_extension(ext) => push(base.to_string()),
        // No extension at all.
        (_, Dialect::TypeScript) => {
            for suffix in ["ts", "tsx", "d.ts", "js", "jsx", "mts", "cts", "mjs", "cjs"] {
                push(format!("{base}.{suffix}"));
            }
            directory_probes(cfg, base, kind, dialect, &mut push);
        }
        (_, Dialect::JavaScript) if kind == ModuleKind::Esm => {
            // A5: exactly the specifier, or nothing. Applying the CommonJS
            // list here invents edges Node would not create.
            push(base.to_string());
        }
        (_, Dialect::JavaScript) => {
            // NODE `LOAD_AS_FILE`, then `LOAD_AS_DIRECTORY`.
            push(base.to_string());
            for suffix in ["js", "json", "node", "mjs", "cjs"] {
                push(format!("{base}.{suffix}"));
            }
            directory_probes(cfg, base, kind, dialect, &mut push);
        }
    }
    out
}

/// NODE `LOAD_AS_DIRECTORY`: the directory's own `package.json` entry point
/// first, then `LOAD_INDEX`.
fn directory_probes(
    cfg: &EcmaConfig,
    base: &str,
    kind: ModuleKind,
    dialect: Dialect,
    push: &mut impl FnMut(String),
) {
    if let Some(scope) = cfg.scopes.iter().find(|s| s.dir == base) {
        for entry in [&scope.types, &scope.module_entry, &scope.main]
            .into_iter()
            .flatten()
        {
            let Some(target) = join_normalized(base, entry) else {
                continue;
            };
            if extension(&target).is_some_and(is_code_extension) || target.ends_with(".d.ts") {
                push(target);
            } else {
                for suffix in index_suffixes(dialect) {
                    push(format!("{target}.{suffix}"));
                    push(under(&target, &format!("index.{suffix}")));
                }
            }
        }
    }
    let _ = kind;
    for suffix in index_suffixes(dialect) {
        push(under(base, &format!("index.{suffix}")));
    }
}

/// Join a repo-relative directory with a file name, without minting a leading
/// `/` at the repository root.
fn under(dir: &str, file: &str) -> String {
    if dir.is_empty() {
        file.to_string()
    } else {
        format!("{dir}/{file}")
    }
}

fn index_suffixes(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        Dialect::TypeScript => &["ts", "tsx", "d.ts", "js", "jsx", "mjs", "cjs"],
        Dialect::JavaScript => &["js", "json", "node", "mjs", "cjs"],
    }
}

fn is_code_extension(ext: &str) -> bool {
    matches!(
        ext,
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts"
    )
}

/// What a `package.json` map yielded for one subpath.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// A package-relative path.
    Path(String),
    /// The map exists and explicitly refuses this subpath — NODE
    /// `ERR_PACKAGE_PATH_NOT_EXPORTED`, or a `null` target.
    Blocked,
    /// No map, or nothing matched.
    None,
}

/// A12/A8/A4: the entry point a package exposes for one subpath.
fn package_entry(
    scope: &PackageScope,
    subpath: String,
    kind: ModuleKind,
    dialect: Dialect,
) -> Target {
    if let Some(exports) = &scope.exports {
        // A12: self-reference and workspace resolution go through `"exports"`
        // when it is present, and it is exhaustive when it is.
        return subpath_resolve(exports, &subpath, conditions(kind, dialect));
    }
    if subpath == "." {
        // No `"exports"`: `LOAD_AS_DIRECTORY` on the package root, which
        // `directory_probes` performs from the package directory itself.
        return Target::Path("./".to_string());
    }
    // A deep import into a package with no `"exports"` is an ordinary path.
    Target::Path(subpath)
}

/// NODE `PACKAGE_EXPORTS_RESOLVE` / `PACKAGE_IMPORTS_RESOLVE`.
fn subpath_resolve(map: &Json, subpath: &str, conds: &[&str]) -> Target {
    // Sugar: a string (or a conditions object) at the top level means `{".": …}`.
    let is_subpath_map = matches!(map, Json::Object { order, .. }
        if order.iter().all(|k| k.starts_with('.') || k.starts_with('#')));
    if !is_subpath_map {
        if subpath != "." {
            return Target::Blocked;
        }
        return target_resolve(map, "", conds);
    }
    if let Some(exact) = map.get(subpath) {
        return target_resolve(exact, "", conds);
    }
    // A10 `PATTERN_KEY_COMPARE`: candidate keys ordered by specificity —
    // longest base before the `*`, then longest suffix after it. Not
    // first-match-in-file-order, which would resolve `./features/x` through
    // `./*` when `./features/*` was written for it.
    let mut best: Option<(usize, usize, &Json, String)> = None;
    for (key, value) in map.entries() {
        let Some((prefix, suffix)) = key.split_once('*') else {
            continue;
        };
        if !subpath.starts_with(prefix) || !subpath.ends_with(suffix) {
            continue;
        }
        if subpath.len() < prefix.len() + suffix.len() {
            continue;
        }
        let matched = subpath[prefix.len()..subpath.len() - suffix.len()].to_string();
        let better = best.as_ref().is_none_or(|(p, s, _, _)| {
            prefix.len() > *p || (prefix.len() == *p && suffix.len() > *s)
        });
        if better {
            best = Some((prefix.len(), suffix.len(), value, matched));
        }
    }
    match best {
        Some((_, _, value, matched)) => target_resolve(value, &matched, conds),
        // A8: the map is exhaustive, so an unlisted subpath is refused rather
        // than falling back to the file.
        None => Target::Blocked,
    }
}

/// NODE `PACKAGE_TARGET_RESOLVE`, with `*` substitution.
fn target_resolve(target: &Json, matched: &str, conds: &[&str]) -> Target {
    match target {
        Json::String(s) => Target::Path(s.replace('*', matched)),
        // An explicit `null` blocks the subpath.
        Json::Null => Target::Blocked,
        Json::Array(items) => {
            for item in items {
                match target_resolve(item, matched, conds) {
                    Target::None => continue,
                    hit => return hit,
                }
            }
            Target::None
        }
        Json::Object { .. } => {
            // Conditions match in **object key order**, and the resolver
            // supplies the set — so the importing file decides which file a
            // dual-published package resolves to.
            for (key, value) in target.entries() {
                if conds.contains(&key) {
                    match target_resolve(value, matched, conds) {
                        Target::None => continue,
                        hit => return hit,
                    }
                }
            }
            Target::None
        }
        _ => Target::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track_ecma::json::parse;
    use crate::track_ecma::project::build;
    use std::fs;
    use std::path::Path;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn candidates(spec: Spec) -> Vec<String> {
        match spec {
            Spec::Candidates { paths, .. } => paths,
            other => panic!("expected candidates, got {other:?}"),
        }
    }

    #[test]
    fn builtins_and_urls_are_external_never_unresolved() {
        let cfg = EcmaConfig::default();
        for (spec, want) in [
            ("fs", "node:fs"),
            ("node:test", "node:test"),
            ("node:fs/promises", "node:fs/promises"),
            ("https://esm.sh/x", "url:https"),
            ("data:text/js,1", "url:data"),
        ] {
            assert_eq!(
                resolve(
                    &cfg,
                    "a.js",
                    spec,
                    ModuleKind::CommonJs,
                    Dialect::JavaScript
                ),
                Spec::External(want.to_string()),
                "{spec}"
            );
        }
    }

    #[test]
    fn commonjs_probes_and_esm_does_not() {
        let cfg = EcmaConfig::default();
        // NODE `LOAD_AS_FILE` then `LOAD_INDEX`.
        let cjs = candidates(resolve(
            &cfg,
            "src/a.js",
            "./util",
            ModuleKind::CommonJs,
            Dialect::JavaScript,
        ));
        assert_eq!(cjs[0], "src/util");
        assert!(cjs.contains(&"src/util.js".to_string()));
        assert!(cjs.contains(&"src/util/index.js".to_string()));

        // A5: ESM resolves `./util` to `./util` exactly or fails. Probing
        // here would invent edges Node would not create.
        let esm = candidates(resolve(
            &cfg,
            "src/a.mjs",
            "./util",
            ModuleKind::Esm,
            Dialect::JavaScript,
        ));
        assert_eq!(esm, ["src/util"]);
    }

    #[test]
    fn typescript_rewrites_the_output_extension_and_prefers_ts_over_d_ts() {
        let cfg = EcmaConfig::default();
        let probes = candidates(resolve(
            &cfg,
            "src/a.ts",
            "./x.js",
            ModuleKind::Esm,
            Dialect::TypeScript,
        ));
        assert_eq!(probes[0], "src/x.ts");
        let ts = probes.iter().position(|p| p == "src/x.ts").unwrap();
        let dts = probes.iter().position(|p| p == "src/x.d.ts").unwrap();
        assert!(ts < dts, "`.ts` is authoritative over `.d.ts`");
        assert!(probes.contains(&"src/x.js".to_string()));
    }

    #[test]
    fn tsconfig_paths_map_a_bare_specifier_to_workspace_source() {
        // The vue-core case: `@vue/*` → `packages/*/src`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@vue/*":["packages/*/src"]}}}"#,
        );
        write(root, "packages/reactivity/src/index.ts", "");
        let cfg = build(root);
        let probes = candidates(resolve(
            &cfg,
            "packages/runtime-core/src/a.ts",
            "@vue/reactivity",
            ModuleKind::Esm,
            Dialect::TypeScript,
        ));
        assert_eq!(probes[0], "packages/reactivity/src.ts");
        assert!(probes.contains(&"packages/reactivity/src/index.ts".to_string()));
    }

    #[test]
    fn the_longest_matching_path_prefix_wins() {
        let paths = vec![
            ("@app/*".to_string(), vec!["src/*".to_string()]),
            ("@app/ui/*".to_string(), vec!["ui/*".to_string()]),
        ];
        assert_eq!(paths_substitutions(&paths, "@app/ui/Button"), ["ui/Button"]);
        assert_eq!(paths_substitutions(&paths, "@app/util"), ["src/util"]);
        assert!(paths_substitutions(&paths, "other").is_empty());
    }

    #[test]
    fn exports_are_exhaustive_and_conditional() {
        let exports = parse(
            r#"{".":{"import":"./src/i.mjs","require":"./dist/i.cjs"},"./parse":"./src/parse.js"}"#,
        )
        .unwrap();
        let esm = &["node", "import", "default"][..];
        let cjs = &["node", "require", "default"][..];
        // A9: the *importer* decides, so one package resolves two ways.
        assert_eq!(
            subpath_resolve(&exports, ".", esm),
            Target::Path("./src/i.mjs".into())
        );
        assert_eq!(
            subpath_resolve(&exports, ".", cjs),
            Target::Path("./dist/i.cjs".into())
        );
        assert_eq!(
            subpath_resolve(&exports, "./parse", esm),
            Target::Path("./src/parse.js".into())
        );
        // A8: present but unlisted is a refusal, not a fallback to the file.
        assert_eq!(subpath_resolve(&exports, "./secret", esm), Target::Blocked);
    }

    #[test]
    fn subpath_patterns_order_by_specificity_and_null_blocks() {
        let exports =
            parse(r#"{"./*":"./src/*.js","./features/*":"./src/features/*.js","./x":null}"#)
                .unwrap();
        let conds = &["default"][..];
        assert_eq!(
            subpath_resolve(&exports, "./features/a", conds),
            Target::Path("./src/features/a.js".into()),
        );
        assert_eq!(
            subpath_resolve(&exports, "./b", conds),
            Target::Path("./src/b.js".into()),
        );
        assert_eq!(subpath_resolve(&exports, "./x", conds), Target::Blocked);
    }

    #[test]
    fn a_workspace_member_stays_internal_and_a_dependency_is_external() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "package.json",
            r#"{"name":"root","dependencies":{"lodash":"^4"}}"#,
        );
        write(
            root,
            "packages/ui/package.json",
            r#"{"name":"@app/ui","main":"./src/index.js"}"#,
        );
        write(root, "packages/ui/src/index.js", "");
        let cfg = build(root);

        let probes = candidates(resolve(
            &cfg,
            "app/main.js",
            "@app/ui",
            ModuleKind::CommonJs,
            Dialect::JavaScript,
        ));
        assert!(
            probes.contains(&"packages/ui/src/index.js".to_string()),
            "{probes:?}"
        );

        assert_eq!(
            resolve(
                &cfg,
                "app/main.js",
                "lodash",
                ModuleKind::CommonJs,
                Dialect::JavaScript
            ),
            Spec::External("npm:lodash".to_string()),
        );
        // Neither declared nor present: honest, and the same answer Go gives.
        assert_eq!(
            resolve(
                &cfg,
                "app/main.js",
                "ghost",
                ModuleKind::CommonJs,
                Dialect::JavaScript
            ),
            Spec::Unresolved(UnresolvedReason::UnknownPackage),
        );
    }

    #[test]
    fn subpath_imports_resolve_against_the_importers_own_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "package.json",
            r##"{"name":"app","imports":{"#db":"./src/db.js"}}"##,
        );
        write(root, "src/db.js", "");
        let cfg = build(root);
        let probes = candidates(resolve(
            &cfg,
            "src/a.js",
            "#db",
            ModuleKind::CommonJs,
            Dialect::JavaScript,
        ));
        assert_eq!(probes[0], "src/db.js");
    }

    #[test]
    fn assets_are_external_and_queries_are_stripped() {
        let cfg = EcmaConfig::default();
        assert_eq!(
            resolve(
                &cfg,
                "src/a.ts",
                "./logo.svg?url",
                ModuleKind::Esm,
                Dialect::TypeScript
            ),
            Spec::External("asset:svg".to_string()),
        );
        assert_eq!(
            resolve(
                &cfg,
                "src/a.js",
                "./data.json",
                ModuleKind::CommonJs,
                Dialect::JavaScript
            ),
            Spec::External("asset:json".to_string()),
        );
    }

    #[test]
    fn a_specifier_escaping_the_repository_is_module_not_found() {
        let cfg = EcmaConfig::default();
        assert_eq!(
            resolve(
                &cfg,
                "a.js",
                "../../outside",
                ModuleKind::CommonJs,
                Dialect::JavaScript
            ),
            Spec::Unresolved(UnresolvedReason::ModuleNotFound),
        );
    }

    #[test]
    fn skipped_directories_are_never_probed() {
        // A path the walk never reads can never be a module node, so probing
        // it would add a permanent miss to the invalidation index for an
        // identity nothing will ever declare. Every candidate is filtered
        // out, which leaves nothing to probe.
        let cfg = EcmaConfig::default();
        assert_eq!(
            resolve(
                &cfg,
                "src/a.js",
                "../dist/bundle",
                ModuleKind::CommonJs,
                Dialect::JavaScript,
            ),
            Spec::Unresolved(UnresolvedReason::ModuleNotFound),
        );
    }

    #[test]
    fn the_repository_root_is_a_resolution_target() {
        // `require('..')` from a test directory names the package root, and
        // real CommonJS suites do it constantly. Treating `..` as a bare
        // specifier reports `UnknownPackage` for a directory that is right
        // there.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "package.json", r#"{"name":"app","main":"app.js"}"#);
        write(root, "app.js", "");
        let cfg = build(root);
        for (importer, spec) in [
            ("test/a.js", ".."),
            ("test/a.js", "../"),
            ("test/deep/a.js", "../.."),
        ] {
            let probes = candidates(resolve(
                &cfg,
                importer,
                spec,
                ModuleKind::CommonJs,
                Dialect::JavaScript,
            ));
            assert!(probes.contains(&"app.js".to_string()), "{spec}: {probes:?}");
            assert!(
                probes.iter().all(|p| !p.starts_with('/')),
                "{spec}: a root-relative join grew a leading slash: {probes:?}",
            );
        }
    }

    #[test]
    fn splitting_a_package_name_handles_scopes() {
        assert_eq!(split_package("lodash"), ("lodash", ".".to_string()));
        assert_eq!(split_package("lodash/fp"), ("lodash", "./fp".to_string()));
        assert_eq!(split_package("@vue/core"), ("@vue/core", ".".to_string()));
        assert_eq!(
            split_package("@vue/core/dist/x"),
            ("@vue/core", "./dist/x".to_string())
        );
    }
}
