//! The two-phase scan. Cold indexing is this same code with an empty store.
//!
//! Generic over [`Language`]: every per-language type is an associated type
//! this module moves and never inspects, so no language's manifest, scope,
//! or naming convention is named here.
//!
//! Each phase writes its own half of every file's facts, and each half
//! replaces only itself. That is what makes a re-scan of one file an edit
//! rather than an append, and it is the whole reason the store is addressed
//! per file.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::Outcome;
use crate::extract_go::GoExtractor;
use crate::lang::{
    Entry, Extractor, FileFacts, FileIndex, Language, Resolution, Resolver, SymbolProbe,
};
use crate::model::{DefKind, Definition, Fqn, NodeId, Reference, node_id, reason_code};
use crate::resolve_go::{GoLang, GoResolver};
use crate::store::{
    DeclSite, DefBatch, FileDefs, FileRefs, NodeRecord, RefBatch, RefKey, RefRecord, Report, Store,
    StoredOutcome,
};

/// One changed file, extracted.
struct ChangedFile<L: Language> {
    rel_path: String,
    hash: [u8; 32],
    facts: FileFacts<L>,
}

/// The store's symbol table, as a resolver probes it.
///
/// Lives here rather than beside the trait because the driver is the layer
/// that loads it: a resolver receives a probe, never a table.
impl SymbolProbe for HashMap<NodeId, Entry> {
    fn probe(&self, id: &NodeId) -> Option<Entry> {
        self.get(id).cloned()
    }
}

/// One file's phase-2 facts, accumulated while its references resolve.
#[derive(Default)]
struct RefAcc {
    rows: HashMap<RefKey, RefRecord>,
    edges: BTreeSet<(NodeId, NodeId, u8)>,
    candidates: BTreeSet<(NodeId, RefKey)>,
    /// External identity → (package string, first line reaching it).
    externals: BTreeMap<NodeId, (String, u32)>,
}

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

    // Collect the changed set, in walk order, and everything the walk
    // reached and this scan owns.
    let mut walked: HashSet<String> = HashSet::with_capacity(paths.len());
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
        walked.insert(rel.clone());
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

    // A file the store knows and this walk did not reach is gone: deleted,
    // renamed, or no longer owned because a nested manifest appeared above
    // it. All three mean the same thing to the graph, and dropping its facts
    // before anything is written is what keeps a stale node from surviving
    // as a resolution target.
    let deleted: Vec<String> = store
        .known_files()?
        .into_iter()
        .filter(|known| !walked.contains(known))
        .collect();
    store.forget_files(&deleted)?;

    // Phase 1: definition and container nodes for the changed set.
    let probe = store.symbol_entries()?;
    let mut def_batch = DefBatch {
        files: Vec::with_capacity(changed.len()),
    };
    // The definitions this event declared, by identity. Two of them under
    // one identity is the only case the language can be asked about.
    let mut event_defs: HashMap<NodeId, Vec<Definition>> = HashMap::new();
    for file in &changed {
        let mut nodes = Vec::with_capacity(file.facts.defs.len());
        for def in &file.facts.defs {
            let Some(fqn) = rs.def_fqn(&cfg, &file.facts.header, &def.owner, def, &probe) else {
                continue; // not nameable, so not a node
            };
            let id = node_id(L::DOMAIN, fqn.as_str());
            let declarations = vec![DeclSite {
                file: file.rel_path.clone(),
                line: def.span.line,
            }];
            let record = if def.kind == DefKind::Module {
                // An empty name means "this file does not say", which is not
                // the same as naming the empty string.
                NodeRecord::Package {
                    import_path: fqn.into_string(),
                    name: (!def.name.is_empty()).then(|| def.name.clone()),
                    declarations,
                }
            } else {
                NodeRecord::Definition {
                    fqn: fqn.into_string(),
                    kind: def.kind.code(),
                    declarations,
                }
            };
            nodes.push((id, record));
            event_defs.entry(id).or_default().push(def.clone());
        }
        def_batch.files.push(FileDefs {
            path: file.rel_path.clone(),
            nodes,
        });
    }
    let colliding = store.apply_defs(&def_batch)?;
    let merged = mergeable_count(rs, &colliding, &event_defs);

    // Phase 2: resolve every reference in the changed set. The container
    // names phase 1 just wrote are part of the scope every file is resolved
    // against, so the config is refreshed before any scope is built.
    let probe = store.symbol_entries()?;
    rs.learn_containers(&mut cfg, &store.package_names()?);
    let mut ref_batch = RefBatch {
        files: Vec::with_capacity(changed.len()),
    };
    for file in &changed {
        let scope = rs.scope(&cfg, &file.facts, &probe);
        // The file's container stands in wherever a reference has no
        // nameable encloser, which is where a package-level initialiser's
        // calls belong.
        let container = container_fqn::<L>(rs, &cfg, &file.facts, &probe);
        let container_id = container
            .as_ref()
            .map(|fqn| node_id(L::DOMAIN, fqn.as_str()));
        let container_name = container.as_ref().map_or("", Fqn::as_str);
        let mut acc = RefAcc::default();

        for r in &file.facts.refs {
            let res = rs.resolve(&cfg, &scope, r, &probe);
            // The source of an edge is the reference's nearest nameable
            // encloser, named by the same function that names definitions —
            // so an edge and the node it starts at cannot disagree.
            let enclosing = r
                .enclosing
                .as_ref()
                .and_then(|e| e.as_definition())
                .and_then(|d| rs.def_fqn(&cfg, &file.facts.header, &d.owner, &d, &probe));
            let (src, enclosing_name) = match &enclosing {
                Some(fqn) => (Some(node_id(L::DOMAIN, fqn.as_str())), fqn.as_str()),
                None => (container_id, container_name),
            };
            let key = RefKey {
                file: file.rel_path.clone(),
                kind: r.kind.code(),
                space: r.space.code(),
                enclosing: enclosing_name.to_string(),
                raw_target: r.raw_target.clone(),
                argc: r.argc,
            };
            record::<L>(key, r, src, res, &mut acc);
        }
        ref_batch.files.push(finish(acc, &file.rel_path, file.hash));
    }
    store.apply_refs(&ref_batch)?;

    let mut report = store.report()?;
    report.fqn_collisions = report.fqn_collisions.saturating_sub(merged);
    Ok(report)
}

/// Scan a Go repository. What `main` and the integration tests call.
pub fn scan_go(root: &Path, db_path: &Path) -> Result<Report, String> {
    scan::<GoLang>(root, db_path, &GoExtractor, &GoResolver)
}

/// File one reference's resolution into this file's half: its row, its edge,
/// the external node it reached, and every candidate it probed.
fn record<L: Language>(
    key: RefKey,
    r: &Reference,
    src: Option<NodeId>,
    res: Resolution,
    acc: &mut RefAcc,
) {
    for cand in &res.candidates {
        acc.candidates.insert((*cand, key.clone()));
    }
    let stored = match &res.outcome {
        Outcome::Resolved(id) => {
            if let Some(src) = src {
                acc.edges.insert((src, *id, r.kind.code()));
            }
            StoredOutcome::Resolved(*id)
        }
        Outcome::External(pkg) => {
            // A dependency outside this repository is a node like any other,
            // so a call into one is a real edge rather than a dead end. The
            // `external:` prefix is unreachable by any candidate a resolver
            // generates — no import path or FQN may contain a `:` — so
            // growing the probe set this way cannot change one outcome.
            let id = node_id(L::DOMAIN, &format!("external:{pkg}"));
            if let Some(src) = src {
                acc.edges.insert((src, id, r.kind.code()));
            }
            acc.externals
                .entry(id)
                .and_modify(|(_, line)| *line = (*line).min(r.span.line))
                .or_insert_with(|| (pkg.clone(), r.span.line));
            StoredOutcome::External(pkg.clone())
        }
        Outcome::Unresolved(reason) => StoredOutcome::Unresolved(reason_code(reason)),
    };
    acc.rows
        .entry(key)
        .and_modify(|row| row.count += 1)
        .or_insert(RefRecord {
            outcome: stored,
            count: 1,
            first_line: r.span.line,
            lang: L::LANG.code(),
        });
}

/// Close one file's accumulator into the half the store replaces.
fn finish(acc: RefAcc, path: &str, hash: [u8; 32]) -> FileRefs {
    let mut rows: Vec<(RefKey, RefRecord)> = acc.rows.into_iter().collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let nodes = acc
        .externals
        .into_iter()
        .map(|(id, (package, line))| {
            (
                id,
                NodeRecord::External {
                    package,
                    declarations: vec![DeclSite {
                        file: path.to_string(),
                        line,
                    }],
                },
            )
        })
        .collect();
    FileRefs {
        path: path.to_string(),
        hash,
        nodes,
        rows,
        edges: acc.edges.into_iter().collect(),
        candidates: acc.candidates.into_iter().collect(),
    }
}

/// How many of the identities the store flagged the language itself calls
/// one entity rather than two.
///
/// Only a pair this event holds in full can be asked: a declaration stored
/// by an earlier event kept its FQN, kind and sites, not its [`Definition`],
/// so there is nothing to hand [`Resolver::mergeable`]. Those count as
/// collisions, which is right for every language that answers `false`
/// unconditionally and wrong for the first that does not — and the fix at
/// that point is to store enough of the definition to ask, not to soften the
/// count.
fn mergeable_count<L: Language>(
    rs: &dyn Resolver<L>,
    colliding: &[NodeId],
    event_defs: &HashMap<NodeId, Vec<Definition>>,
) -> u64 {
    let mut merged = 0;
    for id in colliding {
        let Some(defs) = event_defs.get(id) else {
            continue;
        };
        if defs.len() >= 2 && defs.windows(2).all(|pair| rs.mergeable(&pair[0], &pair[1])) {
            merged += 1;
        }
    }
    merged
}

/// The container a file's definitions live in, when it names one.
fn container_fqn<L: Language>(
    rs: &dyn Resolver<L>,
    cfg: &L::Config,
    facts: &FileFacts<L>,
    probe: &dyn SymbolProbe,
) -> Option<Fqn> {
    let def = facts.defs.iter().find(|d| d.kind == DefKind::Module)?;
    rs.def_fqn(cfg, &facts.header, &def.owner, def, probe)
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
