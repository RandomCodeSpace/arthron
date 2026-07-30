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
//! The event's widening lives in this module's memory, and a process can be
//! killed. So the *store* records it too: every transaction that moves an
//! identity withdraws, in that same transaction, the currency claim of every
//! file whose stored resolution consulted it — see [`Store::apply_defs`]. The
//! files it names come back here and join the waking round, so a scan that
//! finishes restores every claim it withdrew and a scan that does not leaves
//! the next one exactly the work it owes.
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
    Entry, Extractor, FileFacts, FileIndex, Language, RefKeyRefinement, Resolution, Resolver,
    Supertypes, SymbolProbe,
};
use crate::model::{DefKind, Definition, Fqn, NodeId, RefKind, Reference, node_id, reason_code};
use crate::registry::REGISTRY;
use crate::resolve_go::{GoLang, GoResolver};
use crate::store::{
    CollisionDisposition, DeclSite, DefBatch, FileDefs, FileError, FileRefs, FileSupers,
    NodePayload, NodeRecord, RefBatch, RefKey, RefRecord, Report, Store, StoredDefinition,
    StoredOutcome, SuperBatch, SuperRecord,
};

/// Files a phase writes in one transaction.
///
/// Bounding the batch bounds one term of a scan's memory, not the scan's:
/// what a batch holds, and the dirty pages redb holds for the transaction
/// writing it, are both freed at the boundary instead of growing to the size
/// of the event. Other structures beside it are still the corpus's — see
/// [`scan`] — so peak RSS stays linear in the tree. Measured on the 2 vCPU
/// reference hardware: 343 MB over a 1.79M-line Go corpus, 676 MB over that
/// same corpus with its owned set doubled. The 512 MB ceiling is an envelope
/// the reference corpus sits well inside, not a structural bound, and it is
/// crossed somewhere near 1.5× that tree.
///
/// 500 because that is the measured shape of the trade: 500 files in one
/// transaction against 216ms as 500 separate ones. Nothing about the graph
/// depends on the number — a file's facts are applied in the same order at
/// any batch size — only the memory and the transaction count do.
const BATCH_FILES: usize = 500;

/// Combine a language's manifest fingerprint with its graph-semantics
/// revision without changing the established revision-zero bytes.
fn graph_fence_digest(manifest_digest: &[u8], graph_revision: u64) -> Vec<u8> {
    if graph_revision == 0 {
        return manifest_digest.to_vec();
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arthron\0graph-revision-fence\0v1\0");
    hasher.update(&(manifest_digest.len() as u64).to_le_bytes());
    hasher.update(manifest_digest);
    hasher.update(&graph_revision.to_le_bytes());
    hasher.finalize().as_bytes().to_vec()
}

/// One file this event re-reads: its identity, and the half of its facts
/// small enough to keep.
///
/// Either its bytes moved, or an identity it referenced did — both mean the
/// same thing to the store: replace this file's halves with what this event
/// says they are.
///
/// The **references are deliberately absent**. Held from the walk until
/// phase 2 consumed them they were, measured over a 5.35M-line Go tree,
/// 89.8% of a cold scan's peak RSS — 729.9 MiB of 813.2 MiB. The
/// declarations are a fifty-fifth of that (13.3 MiB for 72,362 of them) and
/// phase 1 needs them before any file's references are read, so they stay;
/// the references are read again, per file, by whichever later phase wants
/// them, and dropped as soon as it is done. See [`reread`].
///
/// What this record costs is more than those 13.3 MiB, because the path, the
/// hash and the header are per file rather than per declaration — [`scan`]'s
/// `# Memory` section carries the measurement of what the walk ends up
/// holding, which is the number to start from when tuning this.
struct ScannedFile<L: Language> {
    rel_path: String,
    /// Where to read the file again. Phases 1.5 and 2 re-extract from it.
    path: PathBuf,
    hash: [u8; 32],
    /// Language-private facts about the file. Small, and phase 1 needs every
    /// changed file's before it names a single definition.
    header: L::Header,
    /// The declarations phase 1 turns into nodes.
    defs: Vec<Definition>,
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
///
/// # Memory
///
/// A cold scan holds, of the changed set, one file's *references* at a time
/// and every file's declarations. That asymmetry is the whole memory design.
/// References outnumber declarations 23 to 1 on a large Go tree — 1,678,021
/// against 72,362 — and cost 136 bytes plus 5.7 heap allocations each, so
/// holding them all from the walk until phase 2 consumed them was 89.8% of
/// a measured 813.2 MiB peak. They are not held: [`ScannedFile`] keeps the
/// path and the hash, and each later phase reads the file again. See
/// [`reread`].
///
/// Beside that, one phase at a time: the symbol table each phase probes, the
/// FQN index the supertype phase reads, and the two definition maps the
/// widening compares. Each is dropped where the phase that reads it ends,
/// which is why they are dropped by name below rather than at the end of the
/// function.
///
/// Peak RSS is therefore linear in the changed-*file* count and in the
/// declaration count, and in the single largest file, but not in the tree's
/// references. Both per-file terms are real: every changed file keeps a
/// path, a hash and a language header beside its declarations, and the walk
/// keeps another path per *owned* file in `owned`. Measured on the
/// 5.35M-line Go tree, the walk ends at 110,896 kB with the store still
/// untouched, against a 10,240 kB fixed cost — so the 13.3 MiB
/// `docs/decisions.md` accounts to the declarations is one term of a
/// retained set an order of magnitude larger, and the rest of it has not
/// been decomposed. The 512 MB ceiling is checked by
/// `tests/rss_ceiling.rs`, which states exactly what it can and cannot
/// prove — see [`BATCH_FILES`].
pub fn scan<L: Language>(
    root: &Path,
    db_path: &Path,
    ex: &dyn Extractor<L>,
    rs: &dyn Resolver<L>,
    filter: &FileFilter,
) -> Result<Report, String> {
    // The walk's own failures start the list this scan will hand back: a
    // directory it could not descend into is a file set it did not see, and
    // that is the same class of fact as a file it could not read.
    let (paths, walk_errors) = source_files_with::<L>(root, filter)?;
    // Keyed by path so a file that fails twice — once on the walk, once when
    // an event wakes it — is one entry and one count.
    let mut file_errors: BTreeMap<String, String> = walk_errors
        .into_iter()
        .map(|e| (e.path, e.message))
        .collect();
    // The owned files this event could not read. A subset of `file_errors`,
    // and not derivable from it: that map also carries directories the walk
    // could not descend into and paths that are not UTF-8, neither of which
    // is a file the store holds facts under.
    let mut stale: BTreeSet<String> = BTreeSet::new();
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
        let mut report = store.report()?;
        report.file_errors = named(file_errors);
        return Ok(report);
    }

    // Phase 0 could not establish this language's project layout: no manifest
    // where its resolver looks, or one it cannot read. That is this track's
    // precondition and nobody else's — nothing else in the registry was ever
    // asked about this file — so it must not be the whole scan's failure. The
    // track contributes nothing and says so through the same channel that
    // carries every other thing a scan reached and could not turn into facts,
    // keyed by the language rather than by a file, because the missing thing
    // is the project and not a file the walk found. Every other track still
    // runs, and whatever the store already holds for this one is left exactly
    // as the last scan left it: a scan that could not read the layout is in no
    // position to say which of this language's files are gone.
    let mut cfg = match rs.config(root, &index) {
        Ok(cfg) => cfg,
        Err(e) => {
            let store = Store::open(db_path)?;
            let mut report = store.report()?;
            // The rows stay; the *tally* does not. `Store::report` counts
            // whatever the store holds, which after an earlier scan of a tree
            // whose manifest has since broken is a number this run did not
            // measure — and the same document would then carry both that
            // number and the line below saying the track measured nothing.
            // A reader cannot act on a document that disagrees with itself,
            // and `gate --db` on a persistent store would re-base a baseline
            // onto rows no scan produced. Saying nothing about this language
            // is the honest answer: a gate reads a missing tally as nothing
            // to measure and refuses, which is what "measured nothing" means.
            //
            // Forgetting the rows instead would be the *dishonest* answer:
            // this track cannot read the layout, so it is in no position to
            // say which of the language's files are gone.
            report.per_lang.remove(&L::LANG.code());
            file_errors.insert(
                L::LANG.name().to_string(),
                format!(
                    "no {} project here, so this track measured nothing and every \
                     other language's answer is unaffected: {}",
                    L::LANG.name(),
                    e.message,
                ),
            );
            report.file_errors = named(file_errors);
            return Ok(report);
        }
    };
    let store = Store::open(db_path)?;

    // The manifest is a scan input the walk never hashes, and it decides
    // every identity in the graph. Fingerprinting it *before* the config
    // learns anything from the store is what keeps the comparison about the
    // project rather than about what the last scan happened to know.
    let manifest_digest = rs.config_digest(&cfg);
    let fence_digest = graph_fence_digest(&manifest_digest, rs.graph_revision());
    store.fence_config(L::LANG, &fence_digest)?;

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
        // Recorded as reached *before* the read is attempted. A file whose
        // bytes cannot be read is not a file that is gone, and leaving it out
        // of `owned` would put it in the deleted set below — a `chmod 000`
        // would silently forget every fact the file ever produced.
        owned.insert(rel.clone(), path.clone());
        let source = match read_source(path, &rel) {
            Ok(source) => source,
            // Unreadable, or not UTF-8. Named and stepped over: the scan is a
            // measurement of a tree, and one file the filesystem will not hand
            // over is a fact about that tree rather than a reason to measure
            // none of it.
            Err(e) => {
                stale.insert(rel.clone());
                file_errors.insert(rel, e);
                continue;
            }
        };
        let hash = *blake3::hash(source.as_bytes()).as_bytes();
        if store.file_hash(&rel)? == Some(hash) {
            continue; // unchanged: not in this event's changed set
        }
        // `facts.refs` is not moved out, so it is dropped with `facts` at
        // the end of this statement — the walk keeps a file's declarations
        // and forgets its references. That is the whole of the memory fix.
        let facts = extract_facts(ex, &rel, &source);
        changed.push(ScannedFile {
            rel_path: rel,
            path: path.clone(),
            hash,
            header: facts.header,
            defs: facts.defs,
        });
    }

    // Stepping over an unreadable file kept its facts; this is the other
    // half of that trade. Written before the event's first fact rather than
    // with the rest of it, so that a store error later in the scan cannot
    // leave a file both stale and claiming to be current — see
    // [`Store::forget_hashes`].
    flush_stale(&store, &mut stale)?;

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
        if let Some((path, name)) = rs.declared_container(&cfg, &file.header) {
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
    let deleted_before = store.declared_nodes(&deleted)?;
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

    // Every file this event has already invalidated: the store withdrew its
    // currency claim inside the transaction that falsified its answers, and
    // only `apply_refs` puts the claim back. So each of these has to be
    // re-resolved before the scan ends, or the store is left making no claim
    // about a file a completed scan claims everything about — and the next
    // scan would re-read it for nothing. Collected across every phase and
    // folded into the two waking rounds below.
    let deleted_ids: BTreeSet<NodeId> = deleted_before.keys().copied().collect();
    let deleted_paths: BTreeSet<String> = deleted.iter().cloned().collect();
    let mut invalidated = store.declaration_files(&deleted_ids, &deleted_paths)?;
    // A deletion can change a collision disposition without applying a
    // definition half for any survivor. Withdraw those survivors first, so
    // an interruption cannot present the old disposition as current.
    store.forget_hashes(&invalidated.iter().cloned().collect::<Vec<_>>())?;
    invalidated.extend(store.forget_files(&deleted)?);

    // Phase 1: definition and container nodes for the changed set.
    //
    // Written in batches of [`BATCH_FILES`] rather than one transaction over
    // the event. The probe is read once, before the first batch commits, and
    // every file in the phase is named against that one table — so which
    // batch a file lands in decides nothing, and the graph a batched phase
    // leaves is the graph a single transaction left.
    let probe = store.symbol_entries()?;
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
        let defs = phase_one(rs, &cfg, file, &probe);
        for (id, record) in &defs.nodes {
            declared_now.insert(*id, record.payload());
        }
        def_batch.files.push(defs);
        flush_defs(
            &store,
            &mut def_batch,
            &mut flagged,
            &mut invalidated,
            BATCH_FILES,
        )?;
    }
    // The changed set's phase-1 half is applied in full *before* the widening
    // asks what moved. A batch still held here would commit its
    // invalidations after the wake set was chosen, and the files it named
    // would end the scan with their claims withdrawn and their rows stale.
    flush_defs(&store, &mut def_batch, &mut flagged, &mut invalidated, 1)?;

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
    // A woken file that has become unreadable since the walk saw it keeps the
    // halves the last event stored for it — stale, and named here, rather than
    // taking the whole scan down with it.
    let mut to_wake = wake_files(&store, &touched, &covered, &owned)?;
    add_invalidated(&mut to_wake, &invalidated, &covered, &owned);
    let waking: Vec<ScannedFile<L>> = to_wake
        .into_iter()
        .filter_map(|(rel, path)| woken(ex, &path, rel, &mut file_errors, &mut stale))
        .collect();
    // Their definitions cannot have changed — their bytes did not — so this
    // re-asserts an ownership record identical to the one already stored,
    // and keeps every file this event writes phase-2 facts for covered by a
    // phase-1 half.
    for file in &waking {
        def_batch.files.push(phase_one(rs, &cfg, file, &probe));
        flush_defs(
            &store,
            &mut def_batch,
            &mut flagged,
            &mut invalidated,
            BATCH_FILES,
        )?;
    }
    flush_defs(&store, &mut def_batch, &mut flagged, &mut invalidated, 1)?;
    drop(def_batch);

    // Collision semantics belong to the complete current declaration set,
    // never to this event's definitions alone. The store returns sites in
    // deterministic `(file, line)` order; every unordered pair is asked,
    // because adjacent windows accept `field, property, field` while the two
    // fields are incompatible.
    let mut disposition_ids: BTreeSet<NodeId> = declared_before
        .keys()
        .chain(declared_now.keys())
        .copied()
        .collect();
    disposition_ids.extend(flagged);
    let mut dispositions = BTreeMap::new();
    for (id, defs) in store.collision_definitions(&disposition_ids)? {
        let mergeable = (0..defs.len()).all(|left| {
            ((left + 1)..defs.len()).all(|right| rs.mergeable(&defs[left], &defs[right]))
        });
        dispositions.insert(
            id,
            if mergeable {
                CollisionDisposition::Mergeable
            } else {
                CollisionDisposition::Collision
            },
        );
    }
    // Files written by phase 1 still have no currency claim. Persist the
    // verdict before phase 2 can restore any of them.
    store.set_collision_dispositions(&disposition_ids, &dispositions)?;

    // Both halves of the comparison have done their work; `touched` is the
    // whole of what the event kept from them.
    drop(declared_before);
    drop(declared_now);

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
    let mut covered: HashSet<&str> = changed.iter().map(|f| f.rel_path.as_str()).collect();
    covered.extend(waking.iter().map(|f| f.rel_path.as_str()));
    let mut to_rouse: BTreeMap<String, PathBuf> = BTreeMap::new();
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
            // Re-read rather than held: see [`reread`]. A file whose bytes
            // moved under the scan is skipped, and the skip narrows this
            // round: no supertype row of its own is derived, so `supers_now`
            // is missing it, the comparison below under-approximates which
            // identities moved, and a file that should have been roused is
            // not. Bounded and self-correcting rather than silent — the file
            // is in `stale`, so its hash is forgotten and the next scan reads
            // it whole and redoes this comparison. The skip is also per
            // phase, not sticky: bytes that move and move back are skipped
            // here and resolved in phase 2, which leaves this event's
            // supertype rows for that file the previous event's until the
            // next scan replaces them.
            let Some(facts) = reread(ex, file, &mut file_errors, &mut stale) else {
                continue;
            };
            let supers = phase_supers(rs, &cfg, &file.rel_path, &facts, &probe, &fqns, links);
            drop(facts);
            for (id, record) in &supers.types {
                supers_now
                    .entry(*id)
                    .and_modify(|held| held.merge(record.clone()))
                    .or_insert_with(|| record.clone());
            }
            super_batch.files.push(supers);
            flush_supers(&store, &mut super_batch, &mut invalidated, BATCH_FILES)?;
        }
        flush_supers(&store, &mut super_batch, &mut invalidated, 1)?;
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
        to_rouse = wake_files(&store, &moved, &covered, &owned)?;
        // Their phase-1 half is already stored and their bytes did not move,
        // so there is nothing to re-assert: only their references are stale.
        probe.supers = store.supertype_index()?;
    }
    // Outside the branch, because a language with no supertypes still has an
    // event that withdrew claims: whatever phase 1 invalidated and neither
    // round has covered is resolved here, which is the last chance to give
    // those files their claims back.
    add_invalidated(&mut to_rouse, &invalidated, &covered, &owned);
    drop(covered);
    let roused: Vec<ScannedFile<L>> = to_rouse
        .into_iter()
        .filter_map(|(rel, path)| woken(ex, &path, rel, &mut file_errors, &mut stale))
        .collect();

    // Phase 2: resolve every reference in the changed set and in every file
    // this event woke, in either round.
    //
    // A file's references are read here and nowhere else, one file at a
    // time, and dropped before the next is read. Holding the whole changed
    // set's references from the walk to this loop was 89.8% of a cold scan's
    // peak; re-reading the bytes costs one extra parse per file and holds
    // one file's worth. See [`reread`] for why the second parse cannot say
    // anything different from the first.
    let mut ref_batch = RefBatch {
        files: Vec::with_capacity(BATCH_FILES),
    };
    for file in changed.into_iter().chain(waking).chain(roused) {
        let Some(facts) = reread(ex, &file, &mut file_errors, &mut stale) else {
            continue;
        };
        let refs = phase_two(rs, &cfg, &file, &facts, &probe);
        drop(facts);
        drop(file);
        ref_batch.files.push(refs);
        flush_refs(&store, &mut ref_batch, BATCH_FILES)?;
    }
    flush_refs(&store, &mut ref_batch, 1)?;

    // The files the two waking rounds could not read. Their halves are as
    // stale as the walk's were, and for the same reason.
    flush_stale(&store, &mut stale)?;

    let mut report = store.report()?;
    report.file_errors = named(file_errors);
    Ok(report)
}

/// Read and extract one woken file, or record why it could not be.
///
/// A file this event woke and could not read keeps rows the event has just
/// invalidated, so it goes in `stale` exactly as an unreadable file in the
/// walk does.
fn woken<L: Language>(
    ex: &dyn Extractor<L>,
    path: &Path,
    rel: String,
    file_errors: &mut BTreeMap<String, String>,
    stale: &mut BTreeSet<String>,
) -> Option<ScannedFile<L>> {
    match scan_file(ex, path, rel.clone()) {
        Ok(file) => Some(file),
        Err(e) => {
            stale.insert(rel.clone());
            file_errors.insert(rel, e);
            None
        }
    }
}

/// Hand back the store's currency claim for every owned file this event
/// could not read, and empty the set so a later call does not rewrite them.
fn flush_stale(store: &Store, stale: &mut BTreeSet<String>) -> Result<(), String> {
    if stale.is_empty() {
        return Ok(());
    }
    let paths: Vec<String> = std::mem::take(stale).into_iter().collect();
    store.forget_hashes(&paths)
}

/// The accumulated failures, as the report carries them: sorted by path,
/// one entry per file.
fn named(errors: BTreeMap<String, String>) -> Vec<FileError> {
    errors
        .into_iter()
        .map(|(path, message)| FileError { path, message })
        .collect()
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
    invalidated: &mut BTreeSet<String>,
    limit: usize,
) -> Result<(), String> {
    if batch.files.is_empty() || batch.files.len() < limit {
        return Ok(());
    }
    let outcome = store.apply_defs(batch)?;
    flagged.extend(outcome.colliding);
    invalidated.extend(outcome.invalidated);
    batch.files.clear();
    Ok(())
}

/// [`flush_defs`], for the supertype phase.
fn flush_supers(
    store: &Store,
    batch: &mut SuperBatch,
    invalidated: &mut BTreeSet<String>,
    limit: usize,
) -> Result<(), String> {
    if batch.files.is_empty() || batch.files.len() < limit {
        return Ok(());
    }
    invalidated.extend(store.apply_supers(batch)?);
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

/// Fold the files this event has already invalidated into a waking round.
///
/// A file whose claim the store withdrew mid-event must be re-resolved before
/// the event ends: [`Store::apply_refs`] is the only thing that gives the
/// claim back, and a scan that finished without visiting the file would leave
/// a store the next scan re-reads for no reason — and one a cold scan of the
/// same tree would not have written.
///
/// The two filters are `wake_files`'s, for the same two reasons: a file this
/// event already covers is being resolved anyway, and a row whose file the
/// walk did not reach belongs to a deleted file whose facts are already gone.
fn add_invalidated(
    out: &mut BTreeMap<String, PathBuf>,
    invalidated: &BTreeSet<String>,
    already: &HashSet<&str>,
    owned: &BTreeMap<String, PathBuf>,
) {
    for file in invalidated {
        if already.contains(file.as_str()) || out.contains_key(file) {
            continue;
        }
        if let Some(path) = owned.get(file) {
            out.insert(file.clone(), path.clone());
        }
    }
}

/// Extract one file and hand back the slack on the half that is kept.
///
/// A `Vec` doubles, so a file's declarations land in a buffer holding up to
/// twice what the extractor emitted — and the walk holds that buffer, for
/// every changed file at once, until the scan ends. Over a large Go tree the
/// overshoot on the extractor's two vectors measured 99.7 MiB of live
/// capacity holding nothing at all.
///
/// Only `defs` is shrunk here, because on this path only `defs` outlives the
/// statement: the walk drops a file's references where it makes them, so
/// shrinking them would buy a `realloc` and a copy per file for slack
/// nothing was holding. [`reread`] shrinks the references it hands out,
/// because there they are held.
///
/// Capacity is not an observable of the facts: the records, their order and
/// their count are untouched, and no resolver can ask a `Vec` how much room
/// it has. This makes the process smaller, never the graph different.
fn extract_facts<L: Language>(ex: &dyn Extractor<L>, rel_path: &str, source: &str) -> FileFacts<L> {
    let mut facts = ex.extract(rel_path, source);
    facts.defs.shrink_to_fit();
    facts
}

/// Read one file and extract it, as the walk does for a changed file.
fn scan_file<L: Language>(
    ex: &dyn Extractor<L>,
    path: &Path,
    rel_path: String,
) -> Result<ScannedFile<L>, String> {
    let source = read_source(path, &rel_path)?;
    let hash = *blake3::hash(source.as_bytes()).as_bytes();
    let facts = extract_facts(ex, &rel_path, &source);
    Ok(ScannedFile {
        rel_path,
        path: path.to_path_buf(),
        hash,
        header: facts.header,
        defs: facts.defs,
    })
}

/// Read one file again and re-extract it, for a phase that does not hold the
/// walk's references.
///
/// Re-extraction cannot change an outcome, and the enforcement is structural
/// rather than argued: [`Extractor::extract`] takes a path and a string — no
/// probe, no config, no other file — so the same bytes give the same facts.
/// [`scan_file`] already re-reads every woken file on exactly this
/// assumption, so this generalises a path the graph already rests on.
///
/// The bytes are what must be checked, and they are. A file whose hash has
/// moved since the walk is no longer the file this event's phase-1 half
/// describes, and resolving it would place its references against
/// declarations its source no longer makes. It goes to `stale` instead —
/// exactly where an unreadable file goes — so the store withdraws its
/// currency claim and the next scan reads the file whole.
///
/// Every caller must be able to lose a file here, and what each loses
/// differs: phase 2 writes no references for it, and phase 1.5 derives no
/// supertype row for it and so widens over a comparison it is missing from.
/// Both are recoverable for the same reason — the file is in `stale` — and
/// neither is reachable unless the tree is being written while it is read.
fn reread<L: Language>(
    ex: &dyn Extractor<L>,
    file: &ScannedFile<L>,
    file_errors: &mut BTreeMap<String, String>,
    stale: &mut BTreeSet<String>,
) -> Option<FileFacts<L>> {
    let source = match read_source(&file.path, &file.rel_path) {
        Ok(source) => source,
        Err(e) => {
            stale.insert(file.rel_path.clone());
            file_errors.insert(file.rel_path.clone(), e);
            return None;
        }
    };
    if *blake3::hash(source.as_bytes()).as_bytes() != file.hash {
        stale.insert(file.rel_path.clone());
        // The path is the key of this map and the report prints it in front
        // of the message, so the message does not repeat it.
        file_errors.insert(
            file.rel_path.clone(),
            "the bytes changed while this scan was reading the tree, so this file's \
             references are left unresolved and its store rows stale until the next \
             scan"
                .to_owned(),
        );
        return None;
    }
    let mut facts = extract_facts(ex, &file.rel_path, &source);
    // Held, unlike the walk's: this file's references live until the phase
    // that asked for them has resolved them, so the doubling slack on the
    // single largest file is a term of peak RSS. A small one — without this
    // line three cold runs of a 5.35M-line Go tree measured 278,444–286,660 kB
    // against 284,612–286,872 kB over nine runs with it, which is no
    // difference at that spread — but this is the path where the term exists
    // at all, and the walk's, which drops a file's references where it makes
    // them, no longer pays for it.
    facts.refs.shrink_to_fit();
    Some(facts)
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
) -> FileDefs {
    let mut nodes = Vec::with_capacity(file.defs.len());
    for def in &file.defs {
        let Some(fqn) = rs.def_fqn(cfg, &file.header, &def.owner, def, probe) else {
            continue; // not nameable, so not a node
        };
        let id = node_id(L::DOMAIN, fqn.as_str());
        // An empty name means "this file does not say", which is not the
        // same as naming the empty string.
        let targets: Vec<NodeId> = rs
            .def_alias_targets(cfg, &file.header, def, probe)
            .iter()
            .map(|t| node_id(L::DOMAIN, t.as_str()))
            // A self-referential alias is not a forward, and storing it would
            // hand the resolver a one-step cycle to detect at every probe.
            .filter(|t| *t != id)
            .collect();
        let payload = if rs.stores_as_package(def) {
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
            merge_definition: (!rs.stores_as_package(def))
                .then(|| StoredDefinition::from_definition(def)),
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
    rel_path: &str,
    facts: &FileFacts<L>,
    probe: &dyn SymbolProbe,
    fqns: &HashMap<NodeId, String>,
    kinds: &[RefKind],
) -> FileSupers {
    let mut rows: BTreeMap<NodeId, SuperRecord> = BTreeMap::new();
    let complete = || SuperRecord {
        supers: Vec::new(),
        complete: true,
    };
    for def in &facts.defs {
        if def.kind != DefKind::Type {
            continue;
        }
        if let Some(fqn) = rs.def_fqn(cfg, &facts.header, &def.owner, def, probe) {
            rows.entry(node_id(L::DOMAIN, fqn.as_str()))
                .or_insert_with(complete);
        }
    }
    let linking: Vec<&Reference> = facts
        .refs
        .iter()
        .filter(|r| kinds.contains(&r.kind))
        .collect();
    if linking.is_empty() {
        return FileSupers {
            path: rel_path.to_string(),
            types: rows.into_iter().collect(),
        };
    }
    let scope = rs.scope(cfg, facts, probe);
    for r in linking {
        let Some(encloser) = r.enclosing.as_ref().filter(|e| e.kind == DefKind::Type) else {
            continue; // not a fact about a type, so no type's closure moves
        };
        let Some(src) = encloser
            .as_definition()
            .and_then(|d| rs.def_fqn(cfg, &facts.header, &d.owner, &d, probe))
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
        path: rel_path.to_string(),
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
    facts: &FileFacts<L>,
    probe: &dyn SymbolProbe,
) -> FileRefs {
    let scope = rs.scope(cfg, facts, probe);
    // The file's container stands in wherever a reference has no nameable
    // encloser, which is where a package-level initialiser's calls belong.
    let container = container_fqn::<L>(rs, cfg, facts, probe);
    let container_id = container
        .as_ref()
        .map(|fqn| node_id(L::DOMAIN, fqn.as_str()));
    let container_name = container.as_ref().map_or("", Fqn::as_str);
    let mut acc = RefAcc::default();

    for r in &facts.refs {
        let (res, refinement) = rs.resolve_with_key_refinement(cfg, &scope, r, probe);
        // The source of an edge is the reference's nearest nameable
        // encloser, named by the same function that names definitions — so
        // an edge and the node it starts at cannot disagree.
        let enclosing = r
            .enclosing
            .as_ref()
            .and_then(|e| e.as_definition())
            .and_then(|d| rs.def_fqn(cfg, &facts.header, &d.owner, &d, probe));
        let (src, enclosing_name) = match &enclosing {
            Some(fqn) => (Some(node_id(L::DOMAIN, fqn.as_str())), fqn.as_str()),
            None => (container_id, container_name),
        };
        let key = reference_key(&file.rel_path, enclosing_name, r, refinement);
        record::<L>(key, r, src, res, &mut acc);
    }
    finish(acc, &file.rel_path, file.hash)
}

fn reference_key(
    file: &str,
    enclosing: &str,
    reference: &Reference,
    refinement: RefKeyRefinement,
) -> RefKey {
    let arg_types = match refinement {
        RefKeyRefinement::None => None,
        RefKeyRefinement::ArgumentTypes(types) => Some(types),
    };
    RefKey {
        file: file.to_string(),
        kind: reference.kind.code(),
        space: reference.space.code(),
        enclosing: enclosing.to_string(),
        raw_target: reference.raw_target.clone(),
        argc: reference.argc,
        arg_types,
        locally_bound: reference.locally_bound,
    }
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
    // Every live track walks the tree itself, so a directory none of them can
    // descend into is found once per track. Merged by path: the report counts
    // files it could not read, not attempts to read them. Only the last
    // track's report is returned — see below — so without this merge a
    // failure the Go walk found would be gone by the time Python finished.
    let mut file_errors: BTreeMap<String, String> = BTreeMap::new();
    // Languages no track stood behind this run. Only the last track's report
    // is returned and every report is `Store::report`, which counts the whole
    // store — so a language its own track left out of its own report is back
    // in a later track's, with a number nobody claimed. A track is the only
    // authority on its own languages, and this is where that answer is kept
    // until the loop ends.
    let mut unmeasured: Vec<u8> = Vec::new();
    for track in REGISTRY {
        let Some(scan) = track.scan else {
            continue; // not live: owns no file, contributes nothing
        };
        if !config.track_enabled(track.name) {
            // Not asked, so its stored rows stay exactly as the last scan
            // left them. They are not this run's measurement, though: a
            // later live track's full-store report must not re-emit them as
            // one. Name that fact beside omitting the stale tally.
            switched_off = true;
            for lang in track.langs {
                unmeasured.push(lang.code());
                file_errors.insert(
                    lang.name().to_string(),
                    format!(
                        "track `{}` is switched off by {CONFIG_FILE}; this run measured nothing \
                         for {} and retained rows are omitted",
                        track.name,
                        lang.name(),
                    ),
                );
            }
            continue;
        }
        let measured = scan(root, db_path, &filter)?;
        unmeasured.extend(
            track
                .langs
                .iter()
                .map(|lang| lang.code())
                .filter(|code| !measured.per_lang.contains_key(code)),
        );
        file_errors.extend(
            measured
                .file_errors
                .iter()
                .map(|e| (e.path.clone(), e.message.clone())),
        );
        report = Some(measured);
    }
    // Not a default-empty report: "no track is built into this binary" and
    // "every track found nothing" are different facts, and returning zeros
    // for the first would let a gate bless a build that measures nothing.
    // A config that switched every live track off is a third fact, and says
    // so rather than blaming the build.
    let mut report = report.ok_or_else(|| {
        if switched_off {
            format!("every live language track is switched off by {CONFIG_FILE}")
        } else {
            "no language track is enabled in this build".to_string()
        }
    })?;
    for code in unmeasured {
        report.per_lang.remove(&code);
    }
    report.file_errors = named(file_errors);
    Ok(report)
}

#[cfg(test)]
mod ref_key_refinement_tests {
    use super::*;
    use crate::lang::RefKeyRefinement;
    use crate::model::{DeclSpace, RefTarget, Span, TargetRoot};

    fn typed_reference() -> Reference {
        Reference {
            kind: RefKind::Call,
            space: DeclSpace::Value,
            raw_target: "pick".to_string(),
            target: RefTarget {
                root: TargetRoot::Name,
                segments: vec!["pick".to_string()],
            },
            locally_bound: false,
            argc: Some(1),
            arg_types: Some(vec!["extractor-type".to_string()]),
            enclosing: None,
            span: Span {
                byte_start: 0,
                byte_end: 4,
                line: 1,
            },
        }
    }

    #[test]
    fn argument_type_refinement_is_the_only_way_types_enter_the_key() {
        let reference = typed_reference();

        let coarse = reference_key("src/A.java", "p#A.m()", &reference, RefKeyRefinement::None);
        assert_eq!(coarse.arg_types, None);

        let refined = reference_key(
            "src/A.java",
            "p#A.m()",
            &reference,
            RefKeyRefinement::ArgumentTypes(vec!["resolver-type".to_string()]),
        );
        assert_eq!(refined.arg_types, Some(vec!["resolver-type".to_string()]));
    }
}

#[cfg(test)]
mod graph_revision_tests {
    use super::*;

    #[test]
    fn revision_zero_preserves_the_manifest_digest() {
        let manifest = b"manifest bytes that already fence stores";
        assert_eq!(graph_fence_digest(manifest, 0), manifest);
    }

    #[test]
    fn nonzero_revision_is_domain_separated() {
        let manifest = b"manifest bytes that already fence stores";
        let first = graph_fence_digest(manifest, 1);
        let manifestless = graph_fence_digest(b"", 1);

        assert_eq!(first, graph_fence_digest(manifest, 1));
        assert_ne!(first, manifest);
        assert_ne!(first, graph_fence_digest(manifest, 2));
        assert_ne!(first, graph_fence_digest(b"different manifest", 1));
        assert!(!manifestless.is_empty());
        assert_eq!(manifestless, graph_fence_digest(b"", 1));
    }
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
                        merge_definition: None,
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
    let (paths, errors) = source_files_with::<L>(root, &FileFilter::none())?;
    // The assertion needs the file set the tree actually holds. A walk that
    // could not read part of it did not produce that set, so this fails
    // rather than quietly asserting completeness over a smaller tree — which
    // is exactly the shape of bug the assertion exists to catch. A scan is
    // the opposite trade and keeps going; see [`source_files_with`].
    match errors.first() {
        Some(first) => Err(format!("{}: {}", first.path, first.message)),
        None => Ok(paths),
    }
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
) -> Result<(Vec<PathBuf>, Vec<FileError>), String> {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    // Resolved once: every link found under this walk is asked the same
    // question about the same tree, and the answer cannot change while the
    // walk runs. `None` when the root will not resolve — the walk is about to
    // fail on it below, and a comparison against nothing would refuse files
    // for a reason that is not theirs.
    let real_root = root.canonicalize().ok();
    for entry in ignore::WalkBuilder::new(root)
        .overrides(filter.overrides())
        .build()
    {
        // A directory the walk may not descend into, a symlink loop, an entry
        // that disappeared between the read and the stat: the walk names it
        // and carries on. Returning here threw away every file already found
        // — one unreadable directory made the whole repository unmeasurable
        // and the report said nothing about which one.
        let entry = match entry {
            Ok(entry) => entry,
            // The scanned root is not an entry the walk can step over: it is
            // the tree. A root that does not exist, or that this process may
            // not read, produced no file set at all, and a report of zeros
            // over no file set is indistinguishable from a clean scan of an
            // empty repository — the shape `scan_repo_with` refuses to
            // return for exactly this reason. So this one failure is fatal
            // and every failure beneath it is data.
            Err(e) if at_root(root, &e) => {
                // `walk_failure`'s message, not the error's: `ignore` prints
                // the path inside it, and this line already opens with it.
                return Err(format!(
                    "{}: {}",
                    root.display(),
                    walk_failure(root, &e).message
                ));
            }
            Err(e) => {
                errors.push(walk_failure(root, &e));
                continue;
            }
        };
        let path = entry.path();
        let owned = path
            .extension()
            .is_some_and(|ext| L::extensions().iter().any(|want| ext == *want));
        if !owned {
            continue;
        }
        // `follow_links` is off, so the walk hands back the link rather than
        // its target — and `is_file` below follows it anyway. A link inside
        // the repository pointing outside it would therefore be read, and its
        // definitions stored under a repo-relative key as though that file
        // were part of this tree. It is not, and a scan is a measurement of
        // *this* tree: every name in the graph is a claim about what is in it.
        //
        // Where the target lands, not whether there is a link. A link whose
        // target is inside the repository is an ordinary file of it and is
        // read as one — the measured Haskell corpus links two of its own
        // modules into a sub-package, and refusing those would drop them and
        // move a committed baseline.
        if entry.path_is_symlink() && escapes_root(real_root.as_deref(), path) {
            errors.push(FileError {
                path: walk_path(root, path),
                message: "a symbolic link whose target is outside the scanned \
                          repository, so whatever is on the other end of it is \
                          not this tree's to claim"
                    .to_string(),
            });
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(root).map_err(|e| e.to_string())?;
        let skipped = rel.components().any(|c| {
            let c = c.as_os_str();
            L::skip_dirs().iter().any(|dir| c == *dir)
        });
        if skipped {
            continue;
        }
        // A path the filesystem will not spell in UTF-8 cannot address facts
        // in the store. Every fact is keyed by its file's repo-relative path,
        // `rel_path` builds that key with `to_string_lossy`, and lossy
        // conversion maps distinct byte sequences onto one string — `a\xFE.go`
        // and `a\xFF.go` become the same key, and whichever is scanned second
        // replaces the first's definitions, edges and rows outright. That is
        // the never-drop rule broken by a filename, so the file is named here
        // and not read, which loses one file's facts visibly instead of
        // another's silently.
        if rel.to_str().is_none() {
            errors.push(FileError {
                path: escaped(rel),
                message: "the path is not valid UTF-8, so it cannot key this \
                          file's facts apart from another's"
                    .to_string(),
            });
            continue;
        }
        out.push(path.to_path_buf());
    }
    // The walk hands files back in the order the filesystem lists them, which
    // differs between machines: the same corpus on a developer box and on a CI
    // runner produced stored graphs that differed by one `Type`. Where two
    // definitions collide on one identity the survivor is whichever was written
    // last, so read order decided the graph's shape — a measurement nobody
    // could reproduce elsewhere. Sorting here makes the file set a property of
    // the tree rather than of the filesystem that holds it. `errors` is left in
    // discovery order: it is a report, and it keys no identity.
    out.sort();
    Ok((out, errors))
}

/// Whether a symbolic link's target lies outside the scanned tree.
///
/// `false` whenever the question cannot be answered — a dangling link, a root
/// that will not resolve. A link pointing at nothing is not a file the
/// `is_file` check accepts either, so nothing is read on that path regardless,
/// and answering "outside" would name a file for a reason nobody established.
fn escapes_root(real_root: Option<&Path>, path: &Path) -> bool {
    let (Some(real_root), Ok(target)) = (real_root, path.canonicalize()) else {
        return false;
    };
    !target.starts_with(real_root)
}

/// Whether a walk failure is about the scanned root itself rather than
/// something under it.
///
/// Matched on the path the error carries, not on the spelling
/// [`walk_failure`] gives it: that function also names the root for a failure
/// that has no path of its own — a symbolic-link loop, a malformed ignore
/// file — and those are failures *within* a tree the walk did reach.
fn at_root(root: &Path, e: &ignore::Error) -> bool {
    matches!(e, ignore::Error::WithPath { path, .. } if path == root)
}

/// A walk failure, split into the path it names and what it says.
///
/// `ignore::Error::WithPath` prints the path *inside* its message, and the
/// report has a column for the path, so the outer layer is peeled rather than
/// printed twice. That layer is what a directory the walk could not descend
/// into arrives as. Anything else — a symbolic-link loop, a malformed ignore
/// file — names the root and keeps its whole message, which already carries
/// whatever paths it is about.
fn walk_failure(root: &Path, e: &ignore::Error) -> FileError {
    match e {
        ignore::Error::WithPath { path, err } => FileError {
            path: walk_path(root, path),
            message: err.to_string(),
        },
        other => FileError {
            path: ".".to_string(),
            message: other.to_string(),
        },
    }
}

/// A path the report can print *and* tell from its neighbours, for a name
/// that is not UTF-8.
///
/// `to_string_lossy` collapses every undecodable byte onto U+FFFD, so two such
/// names print identically — which is the exact confusion the caller is
/// reporting, and reporting it under one spelling would fold the two entries
/// into one and undercount. Rust's `OsStr` debug spelling escapes the raw
/// bytes instead (`util/lossy\xFE.go`), which is distinct per file and shows
/// which byte is the problem. Only the quotes that spelling is wrapped in are
/// dropped.
fn escaped(path: &Path) -> String {
    let shown = format!("{path:?}");
    shown
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(shown.as_str())
        .to_string()
}

/// A walked path as the report spells it: repo-relative when it lies under
/// the scanned root, so it reads like every other path in the report, and the
/// root itself as `.`.
fn walk_path(root: &Path, path: &Path) -> String {
    match rel_path(root, path) {
        Ok(rel) if !rel.is_empty() => rel,
        Ok(_) => ".".to_string(),
        Err(_) => path.display().to_string(),
    }
}
