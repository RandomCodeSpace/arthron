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
//!
//! An event is not only the files whose bytes moved. A definition that
//! appears or disappears changes the answer for references in files nobody
//! edited, and the candidate index — every identity every reference probed,
//! hits and misses alike — is what names them. Those files are re-read and
//! re-resolved in the same event, so the store an incremental scan leaves is
//! the store a cold scan of the same tree would have built.
//!
//! Between the two phases sits a third, for the languages that declare one:
//! the supertype relation. It is the first fact here derived from *two* files
//! at once — a type's bases live in its own file, and what those bases declare
//! lives in theirs — so it is stored per file like every other half, and an
//! edit that moves it wakes the references that read it, by exactly the index
//! a definition edit uses.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::Outcome;
use crate::config::{CONFIG_FILE, Config, FileFilter};
use crate::extract_go::GoExtractor;
use crate::lang::{
    Entry, Extractor, FileFacts, FileIndex, Language, Resolution, Resolver, Supertypes, SymbolProbe,
};
use crate::model::{DefKind, Definition, Fqn, NodeId, RefKind, Reference, node_id, reason_code};
use crate::registry::REGISTRY;
use crate::resolve_go::{GoLang, GoResolver};
use crate::store::{
    DeclSite, DefBatch, FileDefs, FileRefs, FileSupers, NodePayload, NodeRecord, RefBatch, RefKey,
    RefRecord, Report, Store, StoredOutcome, SuperBatch, SuperRecord,
};

/// Files a phase writes in one transaction.
///
/// A phase's batch is the one structure in it whose size is the corpus's, so
/// a whole-event batch makes a cold scan's peak memory the tree's size — and
/// on a large repository that is how the 512 MB ceiling is crossed. Bounding
/// it bounds the phase: what a batch holds, and the dirty pages redb holds
/// for the transaction writing it, are both freed at the boundary.
///
/// 500 because that is the measured shape of the trade: 500 files in one
/// transaction against 216ms as 500 separate ones. Nothing about the graph
/// depends on the number — a file's facts are applied in the same order at
/// any batch size — only the memory and the transaction count do.
const BATCH_FILES: usize = 500;

/// One file this event re-reads, extracted.
///
/// Either its bytes moved, or an identity it referenced did — both mean the
/// same thing to the store: replace this file's halves with what this event
/// says they are.
struct ScannedFile<L: Language> {
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

/// The symbol table plus the supertype relation, as phase 2 probes it.
///
/// Two maps and not one because they are filled by different phases: the
/// entries are what phase 1 stored, and the supertypes are what phase 1.5
/// derived from them. A resolver sees one table and cannot tell.
struct Symbols {
    entries: HashMap<NodeId, Entry>,
    supers: HashMap<NodeId, SuperRecord>,
}

impl SymbolProbe for Symbols {
    fn probe(&self, id: &NodeId) -> Option<Entry> {
        self.entries.get(id).cloned()
    }

    fn supertypes(&self, id: &NodeId) -> Option<Supertypes> {
        self.supers.get(id).map(|record| Supertypes {
            fqns: record.supers.iter().map(Fqn::new).collect(),
            complete: record.complete,
        })
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
///
/// The event then widens, at most twice, and never again after that.
///
/// First on definitions: the identities the changed and deleted files stopped
/// or started declaring name the rows that probed them, those rows name their
/// files, and those files are re-resolved too. Re-reading a file whose bytes
/// did not move cannot change what it *declares*, so that round cannot widen
/// itself.
///
/// Then on supertypes, for a language that has them. What a type extends is
/// not in its identity and not in its payload — rewriting an `extends` clause
/// moves neither — and it decides every member lookup that walks through that
/// type. So the supertype phase compares the rows it just derived with the
/// ones it replaced and wakes the files that consulted an identity whose row
/// moved. That round cannot widen either: which identities exist is settled by
/// the definition phase, and a file's supertypes are a function of its own
/// bytes and that set, so a file this round wakes re-derives exactly the rows
/// it already holds.
pub fn scan<L: Language>(
    root: &Path,
    db_path: &Path,
    ex: &dyn Extractor<L>,
    rs: &dyn Resolver<L>,
    filter: &FileFilter,
) -> Result<Report, String> {
    let paths = source_files_with::<L>(root, filter)?;
    let mut index = FileIndex {
        files: Vec::with_capacity(paths.len()),
    };
    for path in &paths {
        index.files.push(rel_path(root, path)?);
    }
    index.files.sort();

    // A language none of whose files exist here has nothing to scan, and its
    // resolver's config — its manifest — has no reason to exist in this tree
    // either: a Go-less repository owes nobody a `go.mod`. Whatever the store
    // still holds for this language belongs to files that are all gone now,
    // so forget them, and report what the store knows: a track that read
    // nothing has nothing of its own to report.
    if index.files.is_empty() {
        let store = Store::open(db_path)?;
        let orphaned: Vec<String> = store
            .known_files()?
            .into_iter()
            .filter(|file| claims::<L>(file))
            .collect();
        store.forget_files(&orphaned)?;
        return store.report();
    }

    let mut cfg = rs.config(root, &index).map_err(|e| e.message)?;
    let store = Store::open(db_path)?;

    // The manifest is a scan input the walk never hashes, and it decides
    // every identity in the graph. Fingerprinting it *before* the config
    // learns anything from the store is what keeps the comparison about the
    // project rather than about what the last scan happened to know.
    store.fence_config(L::LANG, &rs.config_digest(&cfg))?;

    // Every container name the store already holds. Binding an unaliased
    // import needs a fact out of the *imported* container's source, so a
    // scan that touches one file must still know the names of the packages
    // it did not touch.
    rs.learn_containers(&mut cfg, &store.package_names()?);

    // Collect the changed set, in walk order, and everything the walk
    // reached and this scan owns.
    let mut owned: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut changed: Vec<ScannedFile<L>> = Vec::new();
    for path in &paths {
        let rel = rel_path(root, path)?;
        if !rs.owns_file(&cfg, &rel) {
            continue; // governed by another project; not this scan's file
        }
        let source = read_source(path, &rel)?;
        let hash = *blake3::hash(source.as_bytes()).as_bytes();
        owned.insert(rel.clone(), path.clone());
        if store.file_hash(&rel)? == Some(hash) {
            continue; // unchanged: not in this event's changed set
        }
        let facts = ex.extract(&rel, &source);
        changed.push(ScannedFile {
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
    //
    // Only files carrying an extension *this* language owns are candidates.
    // The store is shared by every enabled track, and a file this walk never
    // looked for is not a file this walk can say is gone. Extension ownership
    // is a partition — see `Lang::for_extension` — so every stored file
    // belongs to exactly one track, and no track can forget another's.
    let deleted: Vec<String> = store
        .known_files()?
        .into_iter()
        .filter(|known| !owned.contains_key(known) && claims::<L>(known))
        .collect();

    // The container names this event's own files decide, folded in before
    // phase 1 rather than after it. The store answers for the files this
    // event did not touch and cannot answer for the ones it did — on a cold
    // scan it answers nothing at all — so without this phase 1 builds
    // identities from names phase 2 then disagrees with.
    let mut event_names: HashMap<String, String> = HashMap::new();
    for file in &changed {
        if let Some((path, name)) = rs.declared_container(&cfg, &file.facts.header) {
            event_names.insert(path, name);
        }
    }
    rs.learn_containers(&mut cfg, &event_names);

    // What this event's own files declared before it ran, read before a
    // single fact is written. After phase 1 has rewritten the ownership
    // records this comparison is against itself, and the affected set comes
    // out empty — which every test whose caller sits in the changed file
    // would still pass.
    let mut event_paths: Vec<String> = changed.iter().map(|f| f.rel_path.clone()).collect();
    event_paths.extend(deleted.iter().cloned());
    let declared_before = store.declared_nodes(&event_paths)?;
    // The other half of what an identity means, read at the same moment and
    // for the same reason: a type whose supertypes move changes what every
    // member lookup below it reaches, while the type's own id and payload sit
    // perfectly still. A language with no link kinds has no such half.
    let links = rs.link_kinds();
    let mut supers_before = if links.is_empty() {
        BTreeMap::new()
    } else {
        store.declared_supers(&event_paths)?
    };

    store.forget_files(&deleted)?;

    // Phase 1: definition and container nodes for the changed set.
    //
    // Written in batches of [`BATCH_FILES`] rather than one transaction over
    // the event. The probe is read once, before the first batch commits, and
    // every file in the phase is named against that one table — so which
    // batch a file lands in decides nothing, and the graph a batched phase
    // leaves is the graph a single transaction left.
    let probe = store.symbol_entries()?;
    // The definitions this event declared, by identity. Two of them under
    // one identity is the only case the language can be asked about.
    let mut event_defs: HashMap<NodeId, Vec<Definition>> = HashMap::new();
    let mut def_batch = DefBatch {
        files: Vec::with_capacity(BATCH_FILES),
    };
    // Every identity any batch flagged as doubly declared. Collected rather
    // than believed: a batch judges the graph as it stands when it commits,
    // and only the finished event can say which of them still stands — see
    // [`Store::definition_collisions`].
    let mut flagged: BTreeSet<NodeId> = BTreeSet::new();
    // Accumulated a batch at a time for the same reason the batch is: this
    // is what the event declares, and it has to outlive the records it was
    // read from.
    let mut declared_now: BTreeMap<NodeId, NodePayload> = BTreeMap::new();
    for file in &changed {
        let defs = phase_one(rs, &cfg, file, &probe, &mut event_defs);
        for (id, record) in &defs.nodes {
            declared_now.insert(*id, record.payload());
        }
        def_batch.files.push(defs);
        flush_defs(&store, &mut def_batch, &mut flagged, BATCH_FILES)?;
    }

    // The identities this event started declaring, stopped declaring, or
    // changed the meaning of. The third case is not the same as the first
    // two: a package's node is its import path, which its directory
    // decides, so rewriting a `package` clause moves no identity at all and
    // still changes what every unaliased import of it binds. Comparing
    // payloads rather than bare ids is what makes that an event.
    //
    // An over-approximation on purpose: an id another, unchanged file also
    // declares still exists, so waking its probers is wasted work rather
    // than a wrong answer, and the narrower test is an existence check the
    // candidate index does not need in order to be correct.
    let touched: Vec<NodeId> = declared_before
        .keys()
        .chain(declared_now.keys())
        .filter(|id| declared_before.get(*id) != declared_now.get(*id))
        .copied()
        .collect::<BTreeSet<NodeId>>()
        .into_iter()
        .collect();
    let covered: HashSet<&str> = changed.iter().map(|f| f.rel_path.as_str()).collect();
    let waking: Vec<ScannedFile<L>> = wake_files(&store, &touched, &covered, &owned)?
        .into_iter()
        .map(|(rel, path)| scan_file(ex, &path, rel))
        .collect::<Result<_, String>>()?;
    // Their definitions cannot have changed — their bytes did not — so this
    // re-asserts an ownership record identical to the one already stored,
    // and keeps every file this event writes phase-2 facts for covered by a
    // phase-1 half.
    for file in &waking {
        def_batch
            .files
            .push(phase_one(rs, &cfg, file, &probe, &mut event_defs));
        flush_defs(&store, &mut def_batch, &mut flagged, BATCH_FILES)?;
    }
    flush_defs(&store, &mut def_batch, &mut flagged, 1)?;
    drop(def_batch);
    // Both halves of the comparison have done their work; `touched` is the
    // whole of what the event kept from them.
    drop(declared_before);
    drop(declared_now);

    let colliding = store.definition_collisions(&flagged)?;
    let merged = mergeable_count(rs, &colliding, &event_defs);
    drop(event_defs);

    // The phase-1 probe goes before the phase-2 one is read: holding two
    // whole symbol tables at once buys nothing, and phase 1 is over.
    drop(probe);
    // The container names phase 1 just wrote are part of the scope every file
    // is resolved against, so the config is refreshed before any scope is
    // built — by phase 1.5 as much as by phase 2.
    let mut probe = Symbols {
        entries: store.symbol_entries()?,
        supers: HashMap::new(),
    };
    rs.learn_containers(&mut cfg, &store.package_names()?);

    // Phase 1.5: the supertype relation. Runs after the definition phase has
    // been *applied*, because a base-class name is placed against the
    // definition table and the changed files' stale definitions are only gone
    // once `apply_defs` has replaced them — an overlay could add this event's
    // definitions but never take the previous event's away, and resolving a
    // base against one of those is a wrong edge rather than a missing one.
    let mut roused: Vec<ScannedFile<L>> = Vec::new();
    if !links.is_empty() {
        // A woken file's bytes did not move and its supertypes still can: the
        // base it names may have become a definition since it was last
        // scanned, which is the very reason it was woken.
        let waking_paths: Vec<String> = waking.iter().map(|f| f.rel_path.clone()).collect();
        for (id, record) in store.declared_supers(&waking_paths)? {
            supers_before
                .entry(id)
                .and_modify(|held| held.merge(record.clone()))
                .or_insert(record);
        }
        let fqns = store.definition_fqns()?;
        // Batched like phase 1, and for the same reason. A file's supertype
        // rows are a function of its own bytes and the definition table this
        // phase never writes to, so a batch boundary moves no row.
        let mut super_batch = SuperBatch {
            files: Vec::with_capacity(BATCH_FILES),
        };
        let mut supers_now: BTreeMap<NodeId, SuperRecord> = BTreeMap::new();
        for file in changed.iter().chain(waking.iter()) {
            let supers = phase_supers(rs, &cfg, file, &probe, &fqns, links);
            for (id, record) in &supers.types {
                supers_now
                    .entry(*id)
                    .and_modify(|held| held.merge(record.clone()))
                    .or_insert_with(|| record.clone());
            }
            super_batch.files.push(supers);
            flush_supers(&store, &mut super_batch, BATCH_FILES)?;
        }
        flush_supers(&store, &mut super_batch, 1)?;
        drop(super_batch);
        drop(fqns);

        // The same widening the definition phase performs, for the same
        // reason and with the same over-approximation: a type whose
        // supertypes moved changes the answer for member references in files
        // nobody edited, and the candidate index names them because
        // consulting the relation at an identity *is* a probe of it.
        //
        // This cannot widen a third time. Which identities exist is settled
        // by `apply_defs`, and a file's supertypes are a function of its own
        // bytes and that set, so a file this round wakes re-derives exactly
        // the rows it already holds.
        let moved: Vec<NodeId> = supers_before
            .keys()
            .chain(supers_now.keys())
            .filter(|id| supers_before.get(*id) != supers_now.get(*id))
            .copied()
            .collect::<BTreeSet<NodeId>>()
            .into_iter()
            .collect();
        let mut covered: HashSet<&str> = changed.iter().map(|f| f.rel_path.as_str()).collect();
        covered.extend(waking.iter().map(|f| f.rel_path.as_str()));
        roused = wake_files(&store, &moved, &covered, &owned)?
            .into_iter()
            .map(|(rel, path)| scan_file(ex, &path, rel))
            .collect::<Result<_, String>>()?;
        // Their phase-1 half is already stored and their bytes did not move,
        // so there is nothing to re-assert: only their references are stale.
        probe.supers = store.supertype_index()?;
    }

    // Phase 2: resolve every reference in the changed set and in every file
    // this event woke, in either round.
    //
    // The files are *consumed* here, not borrowed. Phase 2 is the last thing
    // that reads a file's extracted facts, and on a cold scan those facts are
    // the largest thing the process holds — the whole tree, parsed. Dropping
    // each file's as its half is built is what lets the batches be allocated
    // out of the memory the facts are giving back, rather than beside it.
    let mut ref_batch = RefBatch {
        files: Vec::with_capacity(BATCH_FILES),
    };
    for file in changed.into_iter().chain(waking).chain(roused) {
        let refs = phase_two(rs, &cfg, &file, &probe);
        drop(file);
        ref_batch.files.push(refs);
        flush_refs(&store, &mut ref_batch, BATCH_FILES)?;
    }
    flush_refs(&store, &mut ref_batch, 1)?;

    let mut report = store.report()?;
    report.fqn_collisions = report.fqn_collisions.saturating_sub(merged);
    Ok(report)
}

/// Commit a phase-1 batch and forget it, once it holds `limit` files.
///
/// `limit` is [`BATCH_FILES`] inside a loop and `1` after it, which is the
/// "commit whatever is left" call: an empty batch is never written, because
/// a transaction over no files is a transaction with nothing to say.
///
/// Clearing the batch is the point, not committing it. The batch and the
/// dirty pages redb holds for it are the phase's whole memory, and both go
/// back at this boundary.
fn flush_defs(
    store: &Store,
    batch: &mut DefBatch,
    flagged: &mut BTreeSet<NodeId>,
    limit: usize,
) -> Result<(), String> {
    if batch.files.is_empty() || batch.files.len() < limit {
        return Ok(());
    }
    flagged.extend(store.apply_defs(batch)?);
    batch.files.clear();
    Ok(())
}

/// [`flush_defs`], for the supertype phase.
fn flush_supers(store: &Store, batch: &mut SuperBatch, limit: usize) -> Result<(), String> {
    if batch.files.is_empty() || batch.files.len() < limit {
        return Ok(());
    }
    store.apply_supers(batch)?;
    batch.files.clear();
    Ok(())
}

/// [`flush_defs`], for the reference phase.
fn flush_refs(store: &Store, batch: &mut RefBatch, limit: usize) -> Result<(), String> {
    if batch.files.is_empty() || batch.files.len() < limit {
        return Ok(());
    }
    store.apply_refs(batch)?;
    batch.files.clear();
    Ok(())
}

/// The unchanged files holding a reference that probed one of `touched`.
///
/// Whole files, not individual rows: the index selects the file, and
/// re-resolving one is a parse plus its references through the same per-file
/// replace every changed file already uses. Patching single rows would need
/// sub-file ownership of edges and candidate entries — more machinery, more
/// ways to be subtly wrong, and no measured need.
///
/// A row whose file this event already re-read is dropped here rather than
/// left for a later dedupe: the file is being resolved anyway, and letting it
/// in a second time would make the event replace the same half twice, which
/// is correct only for as long as every pass writes a file's half in full.
/// That is a property to keep, not one to depend on. `already` is that set,
/// and it grows as the event does — the supertype phase can widen it a second
/// time, and the files the first widening chose must not come back.
fn wake_files(
    store: &Store,
    touched: &[NodeId],
    already: &HashSet<&str>,
    owned: &BTreeMap<String, PathBuf>,
) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut out = BTreeMap::new();
    for key in store.rows_for(touched)? {
        if already.contains(key.file.as_str()) {
            continue;
        }
        // A row whose file the walk did not reach belongs to a deleted file,
        // whose facts are already forgotten; nothing is left to re-resolve.
        if let Some(path) = owned.get(&key.file) {
            out.insert(key.file, path.clone());
        }
    }
    Ok(out)
}

/// Read one file and extract it, as the walk does for a changed file.
fn scan_file<L: Language>(
    ex: &dyn Extractor<L>,
    path: &Path,
    rel_path: String,
) -> Result<ScannedFile<L>, String> {
    let source = read_source(path, &rel_path)?;
    let hash = *blake3::hash(source.as_bytes()).as_bytes();
    let facts = ex.extract(&rel_path, &source);
    Ok(ScannedFile {
        rel_path,
        hash,
        facts,
    })
}

fn read_source(path: &Path, rel_path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("reading {rel_path}: {e}"))
}

/// One file's phase-1 half: a node per nameable definition, and the
/// definitions themselves, kept for the one question only the language can
/// answer about two of them sharing an identity.
fn phase_one<L: Language>(
    rs: &dyn Resolver<L>,
    cfg: &L::Config,
    file: &ScannedFile<L>,
    probe: &dyn SymbolProbe,
    event_defs: &mut HashMap<NodeId, Vec<Definition>>,
) -> FileDefs {
    let mut nodes = Vec::with_capacity(file.facts.defs.len());
    for def in &file.facts.defs {
        let Some(fqn) = rs.def_fqn(cfg, &file.facts.header, &def.owner, def, probe) else {
            continue; // not nameable, so not a node
        };
        let id = node_id(L::DOMAIN, fqn.as_str());
        // An empty name means "this file does not say", which is not the
        // same as naming the empty string.
        let targets: Vec<NodeId> = rs
            .def_alias_targets(cfg, &file.facts.header, def, probe)
            .iter()
            .map(|t| node_id(L::DOMAIN, t.as_str()))
            // A self-referential alias is not a forward, and storing it would
            // hand the resolver a one-step cycle to detect at every probe.
            .filter(|t| *t != id)
            .collect();
        let payload = if def.kind == DefKind::Module {
            NodePayload::Package((!def.name.is_empty()).then(|| def.name.clone()))
        } else if targets.is_empty() {
            NodePayload::Definition(def.kind.code(), def.facets.bits())
        } else {
            NodePayload::Alias(def.kind.code(), def.facets.bits(), targets.clone())
        };
        let declarations = vec![DeclSite {
            file: file.rel_path.clone(),
            line: def.span.line,
            payload: payload.clone(),
        }];
        let record = match payload {
            NodePayload::Package(name) => NodeRecord::Package {
                import_path: fqn.into_string(),
                name,
                declarations,
            },
            _ => NodeRecord::Definition {
                fqn: fqn.into_string(),
                kind: def.kind.code(),
                facets: def.facets.bits(),
                targets,
                declarations,
            },
        };
        nodes.push((id, record));
        event_defs.entry(id).or_default().push(def.clone());
    }
    FileDefs {
        path: file.rel_path.clone(),
        nodes,
    }
}

/// One file's supertype half: what each type it declares sits under.
///
/// Every type gets a row, including one that declares nothing above it —
/// "nothing above it" is what makes a member lookup below it a complete
/// search, and it is not the same fact as this scan holding no opinion.
///
/// A supertype reference is filed under its nearest nameable encloser, named
/// by the same [`Resolver::def_fqn`] that named the definitions, so the
/// relation and the node it hangs on cannot drift apart. Anything the
/// resolver did not place at a *definition* — an external base, a package, an
/// unresolved name — leaves the row short and says so, because a resolver
/// that reads a short row as a complete one turns "I could not see it" into
/// "it is not there".
fn phase_supers<L: Language>(
    rs: &dyn Resolver<L>,
    cfg: &L::Config,
    file: &ScannedFile<L>,
    probe: &dyn SymbolProbe,
    fqns: &HashMap<NodeId, String>,
    kinds: &[RefKind],
) -> FileSupers {
    let mut rows: BTreeMap<NodeId, SuperRecord> = BTreeMap::new();
    let complete = || SuperRecord {
        supers: Vec::new(),
        complete: true,
    };
    for def in &file.facts.defs {
        if def.kind != DefKind::Type {
            continue;
        }
        if let Some(fqn) = rs.def_fqn(cfg, &file.facts.header, &def.owner, def, probe) {
            rows.entry(node_id(L::DOMAIN, fqn.as_str()))
                .or_insert_with(complete);
        }
    }
    let linking: Vec<&Reference> = file
        .facts
        .refs
        .iter()
        .filter(|r| kinds.contains(&r.kind))
        .collect();
    if linking.is_empty() {
        return FileSupers {
            path: file.rel_path.clone(),
            types: rows.into_iter().collect(),
        };
    }
    let scope = rs.scope(cfg, &file.facts, probe);
    for r in linking {
        let Some(encloser) = r.enclosing.as_ref().filter(|e| e.kind == DefKind::Type) else {
            continue; // not a fact about a type, so no type's closure moves
        };
        let Some(src) = encloser
            .as_definition()
            .and_then(|d| rs.def_fqn(cfg, &file.facts.header, &d.owner, &d, probe))
            .map(|fqn| node_id(L::DOMAIN, fqn.as_str()))
        else {
            continue; // the subtype is not nameable, so nothing can consult it
        };
        let row = rows.entry(src).or_insert_with(complete);
        match rs.resolve(cfg, &scope, r, probe).outcome {
            Outcome::Resolved(id) => match fqns.get(&id) {
                Some(fqn) => {
                    if !row.supers.contains(fqn) {
                        row.supers.push(fqn.clone());
                    }
                }
                // Resolved, but not to a definition — a package or a module.
                // Nothing to walk into, and the closure below is short.
                None => row.complete = false,
            },
            _ => row.complete = false,
        }
    }
    FileSupers {
        path: file.rel_path.clone(),
        types: rows.into_iter().collect(),
    }
}

/// One file's phase-2 half: every reference resolved against the scope its
/// own header builds, and the rows, edges, external nodes and candidate
/// entries that fall out.
fn phase_two<L: Language>(
    rs: &dyn Resolver<L>,
    cfg: &L::Config,
    file: &ScannedFile<L>,
    probe: &dyn SymbolProbe,
) -> FileRefs {
    let scope = rs.scope(cfg, &file.facts, probe);
    // The file's container stands in wherever a reference has no nameable
    // encloser, which is where a package-level initialiser's calls belong.
    let container = container_fqn::<L>(rs, cfg, &file.facts, probe);
    let container_id = container
        .as_ref()
        .map(|fqn| node_id(L::DOMAIN, fqn.as_str()));
    let container_name = container.as_ref().map_or("", Fqn::as_str);
    let mut acc = RefAcc::default();

    for r in &file.facts.refs {
        let res = rs.resolve(cfg, &scope, r, probe);
        // The source of an edge is the reference's nearest nameable
        // encloser, named by the same function that names definitions — so
        // an edge and the node it starts at cannot disagree.
        let enclosing = r
            .enclosing
            .as_ref()
            .and_then(|e| e.as_definition())
            .and_then(|d| rs.def_fqn(cfg, &file.facts.header, &d.owner, &d, probe));
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
            locally_bound: r.locally_bound,
        };
        record::<L>(key, r, src, res, &mut acc);
    }
    finish(acc, &file.rel_path, file.hash)
}

/// Scan a Go repository, reading every Go file the walk finds.
///
/// What the Go integration tests call directly. [`scan_go_with`] is the same
/// scan under a repository's include/exclude globs, and is what [`REGISTRY`]
/// holds.
pub fn scan_go(root: &Path, db_path: &Path) -> Result<Report, String> {
    scan_go_with(root, db_path, &FileFilter::none())
}

/// Scan a Go repository under a filter. The Go track's registry entry.
pub fn scan_go_with(root: &Path, db_path: &Path, filter: &FileFilter) -> Result<Report, String> {
    scan::<GoLang>(root, db_path, &GoExtractor, &GoResolver, filter)
}

/// Scan a repository with every enabled track in [`REGISTRY`], in registry
/// order. What `main` calls.
///
/// This is the whole of the driver's knowledge of which languages exist: it
/// names none of them. A disabled track is skipped, owns no extension, and so
/// contributes neither a file read nor a row — which is why a build with only
/// Go live measures exactly what `scan_go` alone measured.
///
/// The returned [`Report`] is the last enabled track's, and that is the whole
/// report rather than that track's share of it: [`Store::report`] tallies
/// every row in the store, and each track's rows are already there by the
/// time it runs. Tallies are keyed by language code and stay separate — the
/// report carries one line per language and no combined number exists to
/// return.
pub fn scan_repo(root: &Path, db_path: &Path) -> Result<Report, String> {
    scan_repo_with(root, db_path, &Config::default())
}

/// [`scan_repo`] under a repository's own [`Config`].
///
/// Two things the config may do here and nowhere else: compile the walk's
/// include/exclude globs once for every track, and take a live track out of
/// the run.
///
/// A track the config switches off is skipped entirely — it is not handed an
/// empty file set. The difference matters: a scan forgets the stored files of
/// the extensions the running track owns, so running a track over nothing
/// would delete that language's rows, while skipping it leaves them exactly
/// as the last scan left them. Switching a track off means "do not measure
/// this here", not "erase what was measured".
pub fn scan_repo_with(root: &Path, db_path: &Path, config: &Config) -> Result<Report, String> {
    let filter = config.filter(root)?;
    let mut report = None;
    let mut switched_off = false;
    for track in REGISTRY {
        let Some(scan) = track.scan else {
            continue; // not live: owns no file, contributes nothing
        };
        if !config.track_enabled(track.name) {
            switched_off = true;
            continue;
        }
        report = Some(scan(root, db_path, &filter)?);
    }
    // Not a default-empty report: "no track is built into this binary" and
    // "every track found nothing" are different facts, and returning zeros
    // for the first would let a gate bless a build that measures nothing.
    // A config that switched every live track off is a third fact, and says
    // so rather than blaming the build.
    report.ok_or_else(|| {
        if switched_off {
            format!("every live language track is switched off by {CONFIG_FILE}")
        } else {
            "no language track is enabled in this build".to_string()
        }
    })
}

/// Whether a repo-relative path carries an extension this language owns.
///
/// Matches the test [`source_files`] applies, so the set a scan can forget is
/// exactly the set it can find.
fn claims<L: Language>(rel_path: &str) -> bool {
    Path::new(rel_path)
        .extension()
        .is_some_and(|ext| L::extensions().iter().any(|want| ext == *want))
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
                    package: package.clone(),
                    declarations: vec![DeclSite {
                        file: path.to_string(),
                        line,
                        payload: NodePayload::External(package),
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
    source_files_with::<L>(root, &FileFilter::none())
}

/// [`source_files`] under a repository's include/exclude globs.
///
/// The globs go into the walk rather than filtering its output, so an
/// excluded directory is pruned instead of descended into and thrown away.
///
/// `include` does not prune. A whitelist-only override in the `ignore` crate
/// is applied to files alone — a directory that matches nothing is still
/// descended — so `include = ["src/**"]` walks `node_modules` and rejects its
/// files one at a time. It decides what is *read*, never what is *walked*.
/// Pruning a subtree out of the walk is what `exclude` is for, and the two
/// compose: naming a subtree with `include` and excluding the expensive
/// directories is faster than `include` alone.
pub fn source_files_with<L: Language>(
    root: &Path,
    filter: &FileFilter,
) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .overrides(filter.overrides())
        .build()
    {
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
