//! The two-phase scan. Cold indexing is this same code with an empty store.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::Outcome;
use crate::extract_go::{FileFacts, extract};
use crate::model::{Lang, NodeId, RefKind, node_id, reason_code};
use crate::resolve_go::{FileScope, GoResolver, Resolution, file_scope, parse_go_mod};
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
    let resolver = GoResolver {
        module: module.clone(),
    };
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

    // Phase 1: definitions and package nodes for the changed set.
    let mut phase1 = Batch::default();
    let mut seen_pkgs: HashMap<String, ()> = HashMap::new();
    for file in &changed {
        let pkg_path = resolver.package_path(&file.rel_dir);
        if seen_pkgs.insert(pkg_path.clone(), ()).is_none() {
            phase1.nodes.push((
                node_id(Lang::Go, &pkg_path),
                NodeRecord::Package {
                    import_path: pkg_path.clone(),
                },
            ));
        }
        for def in &file.facts.defs {
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
        let scope: FileScope = file_scope(&module, &file.rel_dir, &file.facts);
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
            let src = match &call.enclosing {
                Some(name) => node_id(Lang::Go, &format!("{}.{name}", scope.pkg_path)),
                None => pkg_node,
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

/// All `.go` files under root, skipping vendor/, testdata/, and any
/// directory governed by a nested go.mod.
fn go_files(root: &Path) -> Result<Vec<PathBuf>, String> {
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
