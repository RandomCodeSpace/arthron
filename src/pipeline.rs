//! The two-phase scan. Cold indexing is this same code with an empty store.
//!
//! Generic over [`Language`]: every per-language type is an associated type
//! this module moves and never inspects, so no language's manifest, scope,
//! or naming convention is named here.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::Outcome;
use crate::extract_go::GoExtractor;
use crate::lang::{Extractor, FileFacts, FileIndex, Language, Resolution, Resolver, SymbolProbe};
use crate::model::{DefKind, NodeId, Reference, node_id, reason_code};
use crate::resolve_go::{GoLang, GoResolver};
use crate::store::{Batch, NodeRecord, RefRecord, Report, Store, StoredOutcome};

/// One changed file, extracted.
struct ChangedFile<L: Language> {
    rel_path: String,
    hash: [u8; 32],
    facts: FileFacts<L>,
}

/// The dedup row key: `(file, kind code, raw target)`.
type RowKey = (String, u8, String);

/// Walk, extract, resolve, store, report. The changed set is exactly the
/// files whose content hash differs from the store — an empty store makes
/// that every file, which is the entire cold/warm distinction.
pub fn scan<L: Language>(
    root: &Path,
    db_path: &Path,
    ex: &dyn Extractor<L>,
    rs: &dyn Resolver<L>,
) -> Result<Report, String> {
    let paths = source_files::<L>(root)?;
    let mut index = FileIndex {
        files: Vec::with_capacity(paths.len()),
    };
    for path in &paths {
        index.files.push(rel_path(root, path)?);
    }
    index.files.sort();
    let mut cfg = rs.config(root, &index).map_err(|e| e.message)?;
    let store = Store::open(db_path)?;

    // Every container name the store already holds. Binding an unaliased
    // import needs a fact out of the *imported* container's source, so a
    // scan that touches one file must still know the names of the packages
    // it did not touch.
    rs.learn_containers(&mut cfg, &store.package_names()?);

    // Collect the changed set, in walk order.
    let mut changed: Vec<ChangedFile<L>> = Vec::new();
    for path in &paths {
        let rel = rel_path(root, path)?;
        if !rs.owns_file(&cfg, &rel) {
            continue; // governed by another project; not this scan's file
        }
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => return Err(format!("reading {rel}: {e}")),
        };
        let hash = *blake3::hash(source.as_bytes()).as_bytes();
        if store.file_hash(&rel)? == Some(hash) {
            continue; // unchanged: not in this event's changed set
        }
        let facts = ex.extract(&rel, &source);
        changed.push(ChangedFile {
            rel_path: rel,
            hash,
            facts,
        });
    }

    // Phase 1: definition and container nodes for the changed set.
    let probe = store.definition_ids()?;
    let mut phase1 = Batch::default();
    // Where each container node sits in `phase1.nodes`, so a second file
    // declaring the same container can supply the name the first one did
    // not carry, rather than pushing a duplicate.
    let mut container_at: HashMap<NodeId, usize> = HashMap::new();
    for file in &changed {
        for def in &file.facts.defs {
            let Some(fqn) = rs.def_fqn(&cfg, &file.facts.header, &def.owner, def, &probe) else {
                continue; // not nameable, so not a node
            };
            let id = node_id(L::DOMAIN, fqn.as_str());
            if def.kind == DefKind::Module {
                // An empty name means "this file does not say", which is not
                // the same as naming the empty string.
                let name = (!def.name.is_empty()).then(|| def.name.clone());
                match container_at.get(&id) {
                    Some(&at) => {
                        if let NodeRecord::Package { name: known, .. } = &mut phase1.nodes[at].1
                            && known.is_none()
                        {
                            *known = name;
                        }
                    }
                    None => {
                        container_at.insert(id, phase1.nodes.len());
                        phase1.nodes.push((
                            id,
                            NodeRecord::Package {
                                import_path: fqn.into_string(),
                                name,
                            },
                        ));
                    }
                }
                continue;
            }
            phase1.nodes.push((
                id,
                NodeRecord::Definition {
                    fqn: fqn.into_string(),
                    kind: def.kind.code(),
                    file: file.rel_path.clone(),
                    line: def.span.line,
                },
            ));
        }
    }
    store.apply(&phase1)?;

    // Phase 2: resolve every reference in the changed set. The container
    // names phase 1 just wrote are part of the scope every file is resolved
    // against, so the config is refreshed before any scope is built.
    let probe = store.definition_ids()?;
    rs.learn_containers(&mut cfg, &store.package_names()?);
    let mut phase2 = Batch::default();
    for file in &changed {
        phase2.files.push((file.rel_path.clone(), file.hash));
        let scope = rs.scope(&cfg, &file.facts, &probe);
        let container = container_node::<L>(rs, &cfg, &file.facts, &probe);
        let mut rows: HashMap<RowKey, RefRecord> = HashMap::new();

        for r in &file.facts.refs {
            let res = rs.resolve(&cfg, &scope, r, &probe);
            // The source of an edge is the reference's nearest nameable
            // encloser, named by the same function that names definitions —
            // so an edge and the node it starts at cannot disagree. With no
            // nameable encloser the file's container stands in, which is
            // where a package-level initialiser's calls belong.
            let src = r
                .enclosing
                .as_ref()
                .and_then(|e| e.as_definition())
                .and_then(|d| rs.def_fqn(&cfg, &file.facts.header, &d.owner, &d, &probe))
                .map(|fqn| node_id(L::DOMAIN, fqn.as_str()))
                .or(container);
            record::<L>(&file.rel_path, r, src, res, &mut rows, &mut phase2);
        }
        for ((f, kind, raw), rec) in rows {
            phase2.refs.push((f, kind, raw, rec));
        }
    }
    store.apply(&phase2)?;

    store.report()
}

/// Scan a Go repository. What `main` and the integration tests call.
pub fn scan_go(root: &Path, db_path: &Path) -> Result<Report, String> {
    scan::<GoLang>(root, db_path, &GoExtractor, &GoResolver)
}

/// File one reference's resolution into the batch: its row, its edge if it
/// resolved, and every candidate it probed.
fn record<L: Language>(
    file: &str,
    r: &Reference,
    src: Option<NodeId>,
    res: Resolution,
    rows: &mut HashMap<RowKey, RefRecord>,
    batch: &mut Batch,
) {
    let key = (file.to_string(), r.kind.code(), r.raw_target.clone());
    for cand in &res.candidates {
        batch.candidates.push((*cand, key.clone()));
    }
    let stored = match &res.outcome {
        Outcome::Resolved(id) => {
            if let Some(src) = src {
                batch.edges.push((src, *id, r.kind.code()));
            }
            StoredOutcome::Resolved(*id)
        }
        Outcome::External(pkg) => StoredOutcome::External(pkg.clone()),
        Outcome::Unresolved(reason) => StoredOutcome::Unresolved(reason_code(reason)),
    };
    rows.entry(key)
        .and_modify(|row| row.count += 1)
        .or_insert(RefRecord {
            outcome: stored,
            count: 1,
            first_line: r.span.line,
            lang: L::LANG.code(),
        });
}

/// The node for the container a file's definitions live in, when it has one.
fn container_node<L: Language>(
    rs: &dyn Resolver<L>,
    cfg: &L::Config,
    facts: &FileFacts<L>,
    probe: &dyn SymbolProbe,
) -> Option<NodeId> {
    let def = facts.defs.iter().find(|d| d.kind == DefKind::Module)?;
    let fqn = rs.def_fqn(cfg, &facts.header, &def.owner, def, probe)?;
    Some(node_id(L::DOMAIN, fqn.as_str()))
}

/// A path under `root`, as a repo-relative `/`-separated string.
fn rel_path(root: &Path, path: &Path) -> Result<String, String> {
    Ok(path
        .strip_prefix(root)
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .replace('\\', "/"))
}

/// Every file under `root` a language owns by extension, skipping the
/// directories it never descends into.
///
/// Public because the completeness assertion — every extracted reference has
/// exactly one stored outcome — has to count references over *this* file set.
/// A second copy of these rules in a test would drift, and the first thing it
/// would hide is a file the scan silently never read.
pub fn source_files<L: Language>(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build() {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let owned = path
            .extension()
            .is_some_and(|ext| L::extensions().iter().any(|want| ext == *want));
        if !owned || !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(root).map_err(|e| e.to_string())?;
        let skipped = rel.components().any(|c| {
            let c = c.as_os_str();
            L::skip_dirs().iter().any(|dir| c == *dir)
        });
        if !skipped {
            out.push(path.to_path_buf());
        }
    }
    Ok(out)
}
