//! The two-phase scan. Cold indexing is this same code with an empty store.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::Outcome;
use crate::extract_go::{FileFacts, extract};
use crate::model::{Lang, NodeId, RefKind, node_id, reason_code};
use crate::resolve_go::{
    FileScope, GoResolver, INIT_FUNC, Resolution, file_scope, import_binding,
    is_external_test_package, is_init_func, package_path_for_file, parse_go_mod,
};
use crate::store::{Batch, NodeRecord, RefRecord, Report, Store, StoredOutcome};

/// One changed file, extracted.
struct ChangedFile {
    rel_path: String,
    rel_dir: String,
    hash: [u8; 32],
    facts: FileFacts,
}

/// Walk, extract, resolve, store, report. The changed set is exactly the
/// files whose content hash differs from the store — an empty store makes
/// that every file, which is the entire cold/warm distinction.
pub fn scan(root: &Path, db_path: &Path) -> Result<Report, String> {
    let go_mod =
        fs::read_to_string(root.join("go.mod")).map_err(|e| format!("reading go.mod: {e}"))?;
    let module =
        parse_go_mod(&go_mod).ok_or_else(|| "go.mod has no module directive".to_string())?;
    let resolver = GoResolver { module };
    let store = Store::open(db_path)?;

    // Collect the changed set.
    let mut changed = Vec::new();
    for path in go_files(root)? {
        let rel = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let source = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => return Err(format!("reading {rel}: {e}")),
        };
        let hash = *blake3::hash(source.as_bytes()).as_bytes();
        if store.file_hash(&rel)? == Some(hash) {
            continue; // unchanged: not in this event's changed set
        }
        let rel_dir = match rel.rsplit_once('/') {
            Some((dir, _)) => dir.to_string(),
            None => String::new(),
        };
        changed.push(ChangedFile {
            rel_path: rel,
            rel_dir,
            hash,
            facts: extract(&source),
        });
    }

    // Declared package names, keyed by import path. An unaliased import
    // binds the imported package's declared name, which is a fact in that
    // package's source rather than in the path — so it is knowable only
    // here, the one layer that sees every package. The store supplies the
    // packages this event did not touch; a changed file overrides it,
    // being the newer evidence.
    let mut package_names = store.package_names()?;
    let mut named_here: HashSet<String> = HashSet::new();
    for file in &changed {
        let Some(declared) = file.facts.package.as_deref() else {
            continue; // no package clause: nothing declared to record
        };
        let pkg_path = resolver.package_path(&file.rel_dir);
        if is_external_test_package(declared, dir_package_name(&package_names, &pkg_path)) {
            continue; // a package of its own, and one nothing may import
        }
        if named_here.insert(pkg_path.clone()) {
            package_names.insert(pkg_path, declared.to_string());
        }
    }

    // The package a file's definitions and same-package candidates belong
    // to. Its directory's, except for an external test file, which is its
    // own package. Recomputed per phase from facts fixed above, so both
    // phases cannot disagree about where a file's definitions went.
    let pkg_path_of = |file: &ChangedFile| -> String {
        let dir_pkg = resolver.package_path(&file.rel_dir);
        let dir_name = dir_package_name(&package_names, &dir_pkg);
        package_path_for_file(&dir_pkg, file.facts.package.as_deref(), dir_name)
    };

    // Phase 1: definitions and package nodes for the changed set.
    let mut phase1 = Batch::default();
    let mut seen_pkgs: HashMap<String, ()> = HashMap::new();
    for file in &changed {
        let pkg_path = pkg_path_of(file);
        if seen_pkgs.insert(pkg_path.clone(), ()).is_none() {
            phase1.nodes.push((
                node_id(Lang::Go, &pkg_path),
                NodeRecord::Package {
                    // An external test package is absent from
                    // `package_names` — nothing may import it — so the file
                    // that declares it is what names it.
                    name: package_names
                        .get(&pkg_path)
                        .cloned()
                        .or_else(|| file.facts.package.clone()),
                    import_path: pkg_path.clone(),
                },
            ));
        }
        for def in &file.facts.defs {
            if is_init_func(def) {
                continue; // nothing can name it, so it is not a node
            }
            let fqn = GoResolver::def_fqn(&pkg_path, def);
            phase1.nodes.push((
                node_id(Lang::Go, &fqn),
                NodeRecord::Definition {
                    fqn,
                    kind: def.kind.code(),
                    file: file.rel_path.clone(),
                    line: def.span.line,
                },
            ));
        }
    }
    store.apply(&phase1)?;

    // Phase 2: resolve every reference in the changed set.
    let symbols = store.definition_ids()?;
    let mut phase2 = Batch::default();
    for file in &changed {
        phase2.files.push((file.rel_path.clone(), file.hash));
        let scope: FileScope =
            file_scope(&resolver, pkg_path_of(file), &file.facts, &package_names);
        let pkg_node = node_id(Lang::Go, &scope.pkg_path);
        let mut rows: HashMap<(String, u8, String), RefRecord> = HashMap::new();

        let record = |raw: String,
                      kind: RefKind,
                      line: u32,
                      src: NodeId,
                      res: Resolution,
                      rows: &mut HashMap<(String, u8, String), RefRecord>,
                      batch: &mut Batch| {
            let key = (file.rel_path.clone(), kind.code(), raw);
            for cand in &res.candidates {
                batch.candidates.push((*cand, key.clone()));
            }
            let stored = match &res.outcome {
                Outcome::Resolved(id) => {
                    batch.edges.push((src, *id, kind.code()));
                    StoredOutcome::Resolved(*id)
                }
                Outcome::External(pkg) => StoredOutcome::External(pkg.clone()),
                Outcome::Unresolved(reason) => StoredOutcome::Unresolved(reason_code(reason)),
            };
            rows.entry(key)
                .and_modify(|r| r.count += 1)
                .or_insert(RefRecord {
                    outcome: stored,
                    count: 1,
                    first_line: line,
                    lang: Lang::Go.code(),
                });
        };

        for imp in &file.facts.imports {
            let res = resolver.resolve_import(&imp.path, &symbols);
            record(
                imp.path.clone(),
                RefKind::Import,
                imp.span.line,
                pkg_node,
                res,
                &mut rows,
                &mut phase2,
            );
        }
        for call in &file.facts.calls {
            let res = resolver.resolve_call(call, &scope, &symbols);
            let src = match call.enclosing.as_deref() {
                // `init` is not a node, so a call inside one hangs off the
                // package itself — the same source a package-level
                // initialiser's calls get.
                None | Some(INIT_FUNC) => pkg_node,
                Some(name) => node_id(Lang::Go, &format!("{}.{name}", scope.pkg_path)),
            };
            record(
                call.raw_target.clone(),
                RefKind::Call,
                call.span.line,
                src,
                res,
                &mut rows,
                &mut phase2,
            );
        }
        for ((f, kind, raw), rec) in rows {
            phase2.refs.push((f, kind, raw, rec));
        }
    }
    store.apply(&phase2)?;

    store.report()
}

/// The package name a directory is known to use: what a file in it
/// declares, or — when no scan has reached one — what its import path
/// suggests.
///
/// Telling `package foo_test` in the directory of package `foo` from a
/// directory whose package genuinely is `foo_test` is what needs this.
fn dir_package_name<'a>(names: &'a HashMap<String, String>, pkg_path: &'a str) -> &'a str {
    names
        .get(pkg_path)
        .map_or_else(|| import_binding(pkg_path), String::as_str)
}

/// All `.go` files under root, skipping vendor/, testdata/, and any
/// directory governed by a nested go.mod.
///
/// Public because the completeness assertion — every extracted reference has
/// exactly one stored outcome — has to count references over *this* file set.
/// A second copy of these rules in a test would drift, and the first thing it
/// would hide is a file the scan silently never read.
pub fn go_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build() {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "go") {
            continue;
        }
        let rel = path.strip_prefix(root).map_err(|e| e.to_string())?;
        let skip = rel.components().any(|c| {
            let c = c.as_os_str();
            c == "vendor" || c == "testdata"
        });
        if skip {
            continue;
        }
        // Nested module: an ancestor dir (excluding root) with its own go.mod.
        let mut dir = path.parent();
        let mut nested = false;
        while let Some(d) = dir {
            if d == root {
                break;
            }
            if d.join("go.mod").is_file() {
                nested = true;
                break;
            }
            dir = d.parent();
        }
        if !nested {
            out.push(path.to_path_buf());
        }
    }
    Ok(out)
}
