//! The durable graph: redb tables, bincode records, batched writes.
//!
//! This layer interprets nothing — it stores what the resolver decided and
//! tallies it back out. Two things it does own: the schema generation, and
//! **file ownership**. Every fact carries the file that produced it, so a
//! re-scan of one file replaces exactly that file's facts and nothing else.
//!
//! Ownership is recorded in two halves — [`Store::apply_defs`] owns nodes,
//! [`Store::apply_refs`] owns rows, edges, candidate entries and the
//! external nodes it materialises — because a single ownership record would
//! make phase 2's replace delete what phase 1 just wrote for the same file,
//! and the symptom would look like "some definitions randomly missing".
//!
//! One write transaction per batch (batch per event: 500 files in one
//! transaction measured 60ms against 216ms as 500 separate transactions).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use bincode::{Decode, Encode, config};
use redb::{
    Database, MultimapTableDefinition, ReadableDatabase, ReadableMultimapTable, ReadableTable,
    TableDefinition,
};

use crate::lang::Entry;
use crate::model::{DefFacets, DefKind, NodeId};

/// On-disk schema generation.
///
/// A store written under any other value is dropped and rebuilt rather than
/// migrated: a graph is a cache of facts that can always be recomputed from
/// the source tree, and a half-migrated one is worse than an absent one.
pub const SCHEMA_VERSION: u32 = 2;

/// The [`META`] key the schema generation is stored under.
const SCHEMA_VERSION_KEY: &str = "schema_version";

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const NODES: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("nodes");
const REFS: TableDefinition<(&str, &[u8]), &[u8]> = TableDefinition::new("refs");
const EDGES: TableDefinition<(&[u8; 16], &[u8; 16], u8), ()> = TableDefinition::new("edges");
const REV_EDGES: TableDefinition<(&[u8; 16], &[u8; 16], u8), ()> =
    TableDefinition::new("rev_edges");
const CANDIDATES: MultimapTableDefinition<&[u8; 16], (&str, &[u8])> =
    MultimapTableDefinition::new("candidates");
const FILES: TableDefinition<&str, &[u8; 32]> = TableDefinition::new("files");
const DEF_OWNED: TableDefinition<&str, &[u8]> = TableDefinition::new("def_owned");
const REF_OWNED: TableDefinition<&str, &[u8]> = TableDefinition::new("ref_owned");

type NodeTable<'txn> = redb::Table<'txn, &'static [u8; 16], &'static [u8]>;
type RefTable<'txn> = redb::Table<'txn, (&'static str, &'static [u8]), &'static [u8]>;
type EdgeTable<'txn> = redb::Table<'txn, (&'static [u8; 16], &'static [u8; 16], u8), ()>;
type CandidateTable<'txn> =
    redb::MultimapTable<'txn, &'static [u8; 16], (&'static str, &'static [u8])>;
type BytesTable<'txn> = redb::Table<'txn, &'static str, &'static [u8]>;

/// Where a node is declared: one file, one line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
pub struct DeclSite {
    /// Repo-relative path of the declaring file.
    pub file: String,
    /// 1-based line of the declaration.
    pub line: u32,
}

/// A stored node: something a reference can name.
///
/// Every variant carries its declaration sites rather than a single file,
/// because `redb::insert` overwrites silently and a node two files declare
/// must not lose one of them — that is the never-drop rule applied to nodes.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum NodeRecord {
    /// A definition inside this repository.
    Definition {
        /// Canonical fully-qualified name.
        fqn: String,
        /// [`crate::model::DefKind`] code.
        kind: u8,
        /// Every site declaring it, sorted by `(file, line)`.
        declarations: Vec<DeclSite>,
    },
    /// A package/module inside this repository.
    Package {
        /// The package's import path.
        import_path: String,
        /// The name the package's files declare, when a scanned file in it
        /// declared one. An unaliased import binds *this*, not the last
        /// segment of the import path, and the two differ often enough that
        /// guessing from the path is how a call to an internal package
        /// silently misses.
        name: Option<String>,
        /// Every file declaring membership, sorted by `(file, line)`.
        declarations: Vec<DeclSite>,
    },
    /// A dependency outside this repository.
    External {
        /// The resolver's external string: `std:fmt`, `go:builtin`,
        /// `github.com/pkg/errors`.
        package: String,
        /// Every file that reaches it, at the first line that does, sorted.
        declarations: Vec<DeclSite>,
    },
}

impl NodeRecord {
    /// Every site declaring this node, sorted by `(file, line)`.
    pub fn declarations(&self) -> &[DeclSite] {
        match self {
            NodeRecord::Definition { declarations, .. }
            | NodeRecord::Package { declarations, .. }
            | NodeRecord::External { declarations, .. } => declarations,
        }
    }

    fn declarations_mut(&mut self) -> &mut Vec<DeclSite> {
        match self {
            NodeRecord::Definition { declarations, .. }
            | NodeRecord::Package { declarations, .. }
            | NodeRecord::External { declarations, .. } => declarations,
        }
    }

    fn into_declarations(self) -> Vec<DeclSite> {
        match self {
            NodeRecord::Definition { declarations, .. }
            | NodeRecord::Package { declarations, .. }
            | NodeRecord::External { declarations, .. } => declarations,
        }
    }

    /// Whether two files declare this *definition*.
    ///
    /// Only a definition can collide. A package declared by every file in
    /// its directory is what a package *is*, and an external node reached
    /// from four hundred files is four hundred references, not four hundred
    /// declarations — counting either would drown the signal §3.5 asks for.
    fn is_definition_collision(&self) -> bool {
        let NodeRecord::Definition { declarations, .. } = self else {
            return false;
        };
        let mut files = declarations.iter().map(|d| d.file.as_str());
        match files.next() {
            Some(first) => files.any(|f| f != first),
            None => false,
        }
    }
}

/// Storage mirror of the contract [`crate::Outcome`]. Mirrored so the
/// published contract types carry no serialization derives.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum StoredOutcome {
    /// Linked to a definition in this repository.
    Resolved(NodeId),
    /// Linked to something outside the repository (package ref string).
    External(String),
    /// Not linked; carries a [`crate::model::reason_code`].
    Unresolved(u8),
}

/// The deduplicated reference row key.
///
/// Wider than the site's text on purpose: two calls to `helper()` in two
/// different functions of one file are two rows, because they are two edges
/// from two sources. Collapsing them would make a file's rows unable to
/// express where each edge starts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct RefKey {
    /// Repo-relative path of the file the reference sits in.
    pub file: String,
    /// [`crate::model::RefKind`] code.
    pub kind: u8,
    /// [`crate::model::DeclSpace`] code.
    pub space: u8,
    /// The edge-source FQN this reference sits in — the nearest nameable
    /// encloser's name, or the file's container when there is none. One
    /// string, because a bare name cannot express `C.m`.
    pub enclosing: String,
    /// The literal text at the site.
    pub raw_target: String,
    /// Argument count at a call or creation site, when the extractor
    /// records one. `None` and `Some(0)` are different keys.
    pub argc: Option<u32>,
}

impl RefKey {
    /// Split into the redb key `(file, encoded rest)`.
    ///
    /// The file leads so that every row of one file is one contiguous range,
    /// which is what makes a per-file replace a bounded operation. The rest
    /// is bincode over `(kind, space, enclosing, raw_target, argc)` and is
    /// canonical: one key, one byte string.
    ///
    /// # Panics
    ///
    /// Never in practice: the encoded tuple is two bytes, two strings and an
    /// optional integer, and encoding those into a `Vec` cannot fail.
    pub fn split(&self) -> (&str, Vec<u8>) {
        let rest = (
            self.kind,
            self.space,
            self.enclosing.as_str(),
            self.raw_target.as_str(),
            self.argc,
        );
        let encoded = bincode::encode_to_vec(rest, config::standard())
            .expect("a row key encodes: two bytes, two strings and an optional integer");
        (self.file.as_str(), encoded)
    }

    /// Rebuild a key from the redb pair [`RefKey::split`] produced.
    ///
    /// Trailing bytes are an error rather than ignored padding: an encoding
    /// that accepts two byte strings for one key is not a key at all.
    pub fn join(file: &str, encoded: &[u8]) -> Result<RefKey, String> {
        let ((kind, space, enclosing, raw_target, argc), used): (
            (u8, u8, String, String, Option<u32>),
            usize,
        ) = bincode::decode_from_slice(encoded, config::standard()).map_err(|e| e.to_string())?;
        if used != encoded.len() {
            return Err(format!(
                "row key has {} trailing byte(s) after a complete decode",
                encoded.len() - used
            ));
        }
        Ok(RefKey {
            file: file.to_string(),
            kind,
            space,
            enclosing,
            raw_target,
            argc,
        })
    }
}

/// One deduplicated reference row: outcome, occurrence count, first site.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct RefRecord {
    /// The single outcome for this reference.
    pub outcome: StoredOutcome,
    /// How many times this [`RefKey`] occurs in its file.
    pub count: u32,
    /// 1-based line of the first occurrence.
    pub first_line: u32,
    /// [`crate::model::Lang`] code.
    pub lang: u8,
}

/// Everything phase 1 writes, one entry per file, applied in one transaction.
#[derive(Debug, Clone, Default)]
pub struct DefBatch {
    /// One entry per file the event covers.
    pub files: Vec<FileDefs>,
}

/// One file's phase-1 half.
#[derive(Debug, Clone, Default)]
pub struct FileDefs {
    /// Repo-relative path.
    pub path: String,
    /// The nodes this file declares.
    pub nodes: Vec<(NodeId, NodeRecord)>,
}

/// Everything phase 2 writes, one entry per file, applied in one transaction.
#[derive(Debug, Clone, Default)]
pub struct RefBatch {
    /// One entry per file the event covers.
    pub files: Vec<FileRefs>,
}

/// One file's phase-2 half.
#[derive(Debug, Clone, Default)]
pub struct FileRefs {
    /// Repo-relative path.
    pub path: String,
    /// Content hash, recorded once the file's references are stored.
    pub hash: [u8; 32],
    /// External nodes this file's references materialised.
    pub nodes: Vec<(NodeId, NodeRecord)>,
    /// The file's deduplicated reference rows.
    pub rows: Vec<(RefKey, RefRecord)>,
    /// Edges `(src, dst, kind code)` its resolved references produced.
    pub edges: Vec<(NodeId, NodeId, u8)>,
    /// Candidate-index entries: probed identity → the row that probed it.
    pub candidates: Vec<(NodeId, RefKey)>,
}

/// What phase 1 owns for one file.
#[derive(Debug, Clone, Default, Encode, Decode)]
struct DefOwned {
    nodes: Vec<NodeId>,
}

/// What phase 2 owns for one file. Row and candidate keys are stored as the
/// encoded halves [`RefKey::split`] produced, because that is exactly what a
/// removal needs.
#[derive(Debug, Clone, Default, Encode, Decode)]
struct RefOwned {
    nodes: Vec<NodeId>,
    rows: Vec<Vec<u8>>,
    edges: Vec<(NodeId, NodeId, u8)>,
    candidates: Vec<(NodeId, Vec<u8>)>,
}

/// Per-language resolution tallies, summed over row counts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LangTally {
    /// Occurrences resolved to an in-repo definition.
    pub resolved: u64,
    /// Occurrences linked outside the repo (excluded from the rate).
    pub external: u64,
    /// Occurrences unresolved, keyed by reason code.
    pub unresolved: BTreeMap<u8, u64>,
}

impl LangTally {
    /// Total unresolved occurrences across all reasons.
    pub fn unresolved_total(&self) -> u64 {
        self.unresolved.values().sum()
    }
}

/// Tallies for every language present in the store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// [`crate::model::Lang`] code → tally.
    pub per_lang: BTreeMap<u8, LangTally>,
    /// Distinct FQNs claimed by a definition in more than one file.
    ///
    /// Data, exactly as unresolved references are data: printed, never a
    /// scan failure and never a gate. A build-tag twin pair
    /// (`a_linux.go` and `a_darwin.go` both declaring `func plat()`) is the
    /// common cause, and before this counter existed one of them silently
    /// overwrote the other.
    pub fqn_collisions: u64,
}

/// The whole store as one comparable value: the incremental oracle.
///
/// A report can agree while the graph underneath disagrees — a dangling
/// candidate entry or a node one file too many declares changes no tally.
/// This is what an incremental scan is compared against a cold one with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Content hash per known file.
    pub files: BTreeMap<String, [u8; 32]>,
    /// Every node, by identity.
    pub nodes: BTreeMap<NodeId, NodeRecord>,
    /// Every reference row, by key.
    pub rows: BTreeMap<RefKey, RefRecord>,
    /// Every edge `(src, dst, kind code)`.
    pub edges: BTreeSet<(NodeId, NodeId, u8)>,
    /// The candidate index: probed identity → the rows that probed it.
    pub candidates: BTreeMap<NodeId, BTreeSet<RefKey>>,
}

/// Handle on the on-disk graph.
pub struct Store {
    db: Database,
}

impl Store {
    /// Open (or create) the database and its tables.
    ///
    /// A store carrying any other [`SCHEMA_VERSION`] is wiped: its tables are
    /// dropped and recreated empty, so the next scan sees no known files and
    /// rebuilds everything.
    pub fn open(path: &Path) -> Result<Self, String> {
        let db = Database::create(path).map_err(|e| e.to_string())?;
        let txn = db.begin_write().map_err(|e| e.to_string())?;
        {
            let stored = {
                let meta = txn.open_table(META).map_err(|e| e.to_string())?;
                meta.get(SCHEMA_VERSION_KEY)
                    .map_err(|e| e.to_string())?
                    .and_then(|guard| <[u8; 4]>::try_from(guard.value()).ok())
                    .map(u32::from_le_bytes)
            };
            if stored != Some(SCHEMA_VERSION) {
                txn.delete_table(NODES).map_err(|e| e.to_string())?;
                txn.delete_table(REFS).map_err(|e| e.to_string())?;
                txn.delete_table(EDGES).map_err(|e| e.to_string())?;
                txn.delete_table(REV_EDGES).map_err(|e| e.to_string())?;
                txn.delete_multimap_table(CANDIDATES)
                    .map_err(|e| e.to_string())?;
                txn.delete_table(FILES).map_err(|e| e.to_string())?;
                txn.delete_table(DEF_OWNED).map_err(|e| e.to_string())?;
                txn.delete_table(REF_OWNED).map_err(|e| e.to_string())?;
                let mut meta = txn.open_table(META).map_err(|e| e.to_string())?;
                meta.insert(SCHEMA_VERSION_KEY, &SCHEMA_VERSION.to_le_bytes()[..])
                    .map_err(|e| e.to_string())?;
            }
            // Create every table, so a later read transaction finds them.
            txn.open_table(NODES).map_err(|e| e.to_string())?;
            txn.open_table(REFS).map_err(|e| e.to_string())?;
            txn.open_table(EDGES).map_err(|e| e.to_string())?;
            txn.open_table(REV_EDGES).map_err(|e| e.to_string())?;
            txn.open_multimap_table(CANDIDATES)
                .map_err(|e| e.to_string())?;
            txn.open_table(FILES).map_err(|e| e.to_string())?;
            txn.open_table(DEF_OWNED).map_err(|e| e.to_string())?;
            txn.open_table(REF_OWNED).map_err(|e| e.to_string())?;
        }
        txn.commit().map_err(|e| e.to_string())?;
        Ok(Store { db })
    }

    /// Replace the phase-1 half of every file in the batch, in one
    /// transaction.
    ///
    /// Returns the identities that ended this call with a *definition*
    /// declared in more than one file — the mechanical half of the FQN
    /// grammar's injectivity obligation. The store never judges what that
    /// means: two declarations sharing an FQN are a collision in one
    /// language and one entity in another, and only the language knows.
    pub fn apply_defs(&self, batch: &DefBatch) -> Result<Vec<NodeId>, String> {
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        let mut colliding = Vec::new();
        {
            let mut nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
            let mut owned = txn.open_table(DEF_OWNED).map_err(|e| e.to_string())?;
            let mut touched: BTreeSet<NodeId> = BTreeSet::new();
            for file in &batch.files {
                let previous: DefOwned = read_owned(&owned, &file.path)?.unwrap_or_default();
                for id in &previous.nodes {
                    drop_site(&mut nodes, id, &file.path)?;
                    touched.insert(*id);
                }
                let mut ids = Vec::with_capacity(file.nodes.len());
                let mut seen: HashSet<NodeId> = HashSet::with_capacity(file.nodes.len());
                for (id, record) in &file.nodes {
                    upsert_node(&mut nodes, id, record.clone())?;
                    if seen.insert(*id) {
                        ids.push(*id);
                    }
                    touched.insert(*id);
                }
                write_owned(&mut owned, &file.path, &DefOwned { nodes: ids })?;
            }
            for id in &touched {
                if let Some(record) = read_node(&nodes, id)?
                    && record.is_definition_collision()
                {
                    colliding.push(*id);
                }
            }
        }
        txn.commit().map_err(|e| e.to_string())?;
        Ok(colliding)
    }

    /// Replace the phase-2 half of every file in the batch, in one
    /// transaction, and record each file's content hash.
    pub fn apply_refs(&self, batch: &RefBatch) -> Result<(), String> {
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
            let mut refs = txn.open_table(REFS).map_err(|e| e.to_string())?;
            let mut edges = txn.open_table(EDGES).map_err(|e| e.to_string())?;
            let mut rev = txn.open_table(REV_EDGES).map_err(|e| e.to_string())?;
            let mut cands = txn
                .open_multimap_table(CANDIDATES)
                .map_err(|e| e.to_string())?;
            let mut files = txn.open_table(FILES).map_err(|e| e.to_string())?;
            let mut owned = txn.open_table(REF_OWNED).map_err(|e| e.to_string())?;
            for file in &batch.files {
                forget_ref_half(
                    &mut nodes, &mut refs, &mut edges, &mut rev, &mut cands, &owned, &file.path,
                )?;
                let mut record = RefOwned::default();
                let mut seen: HashSet<NodeId> = HashSet::new();
                for (id, node) in &file.nodes {
                    upsert_node(&mut nodes, id, node.clone())?;
                    if seen.insert(*id) {
                        record.nodes.push(*id);
                    }
                }
                for (key, row) in &file.rows {
                    let (path, encoded) = key.split();
                    let bytes = encode(row)?;
                    refs.insert((path, encoded.as_slice()), bytes.as_slice())
                        .map_err(|e| e.to_string())?;
                    record.rows.push(encoded);
                }
                for (src, dst, kind) in &file.edges {
                    edges
                        .insert((src, dst, *kind), ())
                        .map_err(|e| e.to_string())?;
                    rev.insert((dst, src, *kind), ())
                        .map_err(|e| e.to_string())?;
                    record.edges.push((*src, *dst, *kind));
                }
                for (cand, key) in &file.candidates {
                    let (path, encoded) = key.split();
                    cands
                        .insert(cand, (path, encoded.as_slice()))
                        .map_err(|e| e.to_string())?;
                    record.candidates.push((*cand, encoded));
                }
                write_owned(&mut owned, &file.path, &record)?;
                files
                    .insert(file.path.as_str(), &file.hash)
                    .map_err(|e| e.to_string())?;
            }
        }
        txn.commit().map_err(|e| e.to_string())
    }

    /// Drop every fact these files own, in both halves, plus their hashes.
    ///
    /// A file that stopped being the scan's — a nested manifest appeared
    /// above it — is a deletion by exactly this rule, which is the correct
    /// answer: its facts are no longer this graph's.
    pub fn forget_files(&self, paths: &[String]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
            let mut refs = txn.open_table(REFS).map_err(|e| e.to_string())?;
            let mut edges = txn.open_table(EDGES).map_err(|e| e.to_string())?;
            let mut rev = txn.open_table(REV_EDGES).map_err(|e| e.to_string())?;
            let mut cands = txn
                .open_multimap_table(CANDIDATES)
                .map_err(|e| e.to_string())?;
            let mut files = txn.open_table(FILES).map_err(|e| e.to_string())?;
            let mut def_owned = txn.open_table(DEF_OWNED).map_err(|e| e.to_string())?;
            let mut ref_owned = txn.open_table(REF_OWNED).map_err(|e| e.to_string())?;
            for path in paths {
                forget_ref_half(
                    &mut nodes, &mut refs, &mut edges, &mut rev, &mut cands, &ref_owned, path,
                )?;
                ref_owned.remove(path.as_str()).map_err(|e| e.to_string())?;
                if let Some(previous) = read_owned::<DefOwned>(&def_owned, path)? {
                    for id in &previous.nodes {
                        drop_site(&mut nodes, id, path)?;
                    }
                }
                def_owned.remove(path.as_str()).map_err(|e| e.to_string())?;
                files.remove(path.as_str()).map_err(|e| e.to_string())?;
            }
        }
        txn.commit().map_err(|e| e.to_string())
    }

    /// Every file the store holds facts for.
    pub fn known_files(&self) -> Result<Vec<String>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(FILES).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (key, _) = entry.map_err(|e| e.to_string())?;
            out.push(key.value().to_string());
        }
        Ok(out)
    }

    /// The stored content hash for a file, if any.
    pub fn file_hash(&self, path: &str) -> Result<Option<[u8; 32]>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(FILES).map_err(|e| e.to_string())?;
        Ok(table
            .get(path)
            .map_err(|e| e.to_string())?
            .map(|guard| *guard.value()))
    }

    /// The record stored under a node id, if the node exists.
    ///
    /// The graph's identity surface: what a given FQN hashes to is either a
    /// node or it is not, and this is how that question gets asked.
    pub fn node(&self, id: &NodeId) -> Result<Option<NodeRecord>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(NODES).map_err(|e| e.to_string())?;
        read_node(&table, id)
    }

    /// Whether the graph holds this exact edge.
    ///
    /// An edge is a resolved reference, so `src` is the node the reference
    /// sat in — which is the assertion worth making about it.
    pub fn has_edge(&self, src: &NodeId, dst: &NodeId, kind: u8) -> Result<bool, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(EDGES).map_err(|e| e.to_string())?;
        Ok(table
            .get((src, dst, kind))
            .map_err(|e| e.to_string())?
            .is_some())
    }

    /// Declared package name for every package node that has one, keyed by
    /// import path.
    ///
    /// Binding an unaliased import needs the *imported* package's declared
    /// name, so an event that changes one file must still know the names of
    /// packages it did not touch. The store is where those live.
    pub fn package_names(&self) -> Result<HashMap<String, String>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(NODES).map_err(|e| e.to_string())?;
        let mut out = HashMap::new();
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (_, value) = entry.map_err(|e| e.to_string())?;
            let record: NodeRecord = decode(value.value())?;
            if let NodeRecord::Package {
                import_path,
                name: Some(name),
                ..
            } = record
            {
                out.insert(import_path, name);
            }
        }
        Ok(out)
    }

    /// The symbol table a resolver probes: one typed entry per identity.
    ///
    /// Facets are not stored — no shared code branches on them and no
    /// resolver has yet needed one out of the graph — so every definition
    /// answers with the empty set rather than a guess.
    pub fn symbol_entries(&self) -> Result<HashMap<NodeId, Entry>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(NODES).map_err(|e| e.to_string())?;
        let mut out = HashMap::new();
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            let record: NodeRecord = decode(value.value())?;
            let typed = match record {
                NodeRecord::Definition { kind, .. } => Entry::Definition {
                    kind: DefKind::from_code(kind)
                        .ok_or_else(|| format!("stored node kind {kind} has no variant"))?,
                    facets: DefFacets::default(),
                },
                NodeRecord::Package { .. } => Entry::Container,
                NodeRecord::External { .. } => Entry::External,
            };
            out.insert(*key.value(), typed);
        }
        Ok(out)
    }

    /// Every reference row that probed this identity, hit or miss.
    ///
    /// The misses are the point: a reference that probed an identity and
    /// found nothing is exactly the one an edit declaring it must wake.
    pub fn candidate_rows(&self, id: &NodeId) -> Result<Vec<RefKey>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn
            .open_multimap_table(CANDIDATES)
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for value in table.get(id).map_err(|e| e.to_string())? {
            let guard = value.map_err(|e| e.to_string())?;
            let (file, encoded) = guard.value();
            out.push(RefKey::join(file, encoded)?);
        }
        Ok(out)
    }

    /// The whole store as one comparable value.
    pub fn snapshot(&self) -> Result<Snapshot, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let mut snapshot = Snapshot {
            files: BTreeMap::new(),
            nodes: BTreeMap::new(),
            rows: BTreeMap::new(),
            edges: BTreeSet::new(),
            candidates: BTreeMap::new(),
        };
        let files = txn.open_table(FILES).map_err(|e| e.to_string())?;
        for entry in files.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            snapshot
                .files
                .insert(key.value().to_string(), *value.value());
        }
        let nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
        for entry in nodes.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            snapshot.nodes.insert(*key.value(), decode(value.value())?);
        }
        let refs = txn.open_table(REFS).map_err(|e| e.to_string())?;
        for entry in refs.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            let (file, encoded) = key.value();
            snapshot
                .rows
                .insert(RefKey::join(file, encoded)?, decode(value.value())?);
        }
        let edges = txn.open_table(EDGES).map_err(|e| e.to_string())?;
        for entry in edges.iter().map_err(|e| e.to_string())? {
            let (key, _) = entry.map_err(|e| e.to_string())?;
            let (src, dst, kind) = key.value();
            snapshot.edges.insert((*src, *dst, kind));
        }
        let cands = txn
            .open_multimap_table(CANDIDATES)
            .map_err(|e| e.to_string())?;
        for entry in cands.iter().map_err(|e| e.to_string())? {
            let (key, values) = entry.map_err(|e| e.to_string())?;
            let mut rows = BTreeSet::new();
            for value in values {
                let guard = value.map_err(|e| e.to_string())?;
                let (file, encoded) = guard.value();
                rows.insert(RefKey::join(file, encoded)?);
            }
            snapshot.candidates.insert(*key.value(), rows);
        }
        Ok(snapshot)
    }

    /// Tally every reference row into per-language counts, and count the
    /// FQNs two files' definitions both claim.
    pub fn report(&self) -> Result<Report, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let mut report = Report::default();
        let refs = txn.open_table(REFS).map_err(|e| e.to_string())?;
        for entry in refs.iter().map_err(|e| e.to_string())? {
            let (_, value) = entry.map_err(|e| e.to_string())?;
            let record: RefRecord = decode(value.value())?;
            let tally = report.per_lang.entry(record.lang).or_default();
            match record.outcome {
                StoredOutcome::Resolved(_) => tally.resolved += u64::from(record.count),
                StoredOutcome::External(_) => tally.external += u64::from(record.count),
                StoredOutcome::Unresolved(reason) => {
                    *tally.unresolved.entry(reason).or_default() += u64::from(record.count);
                }
            }
        }
        // Derived from the stored nodes rather than from the last event, so
        // the number is a property of the graph: a warm scan that touched
        // one file reports the same count a cold scan of the same tree does.
        let nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
        for entry in nodes.iter().map_err(|e| e.to_string())? {
            let (_, value) = entry.map_err(|e| e.to_string())?;
            let record: NodeRecord = decode(value.value())?;
            if record.is_definition_collision() {
                report.fqn_collisions += 1;
            }
        }
        Ok(report)
    }
}

fn encode<T: Encode>(value: &T) -> Result<Vec<u8>, String> {
    bincode::encode_to_vec(value, config::standard()).map_err(|e| e.to_string())
}

fn decode<T: Decode<()>>(bytes: &[u8]) -> Result<T, String> {
    let (value, _) = bincode::decode_from_slice(bytes, config::standard())
        .map_err(|e: bincode::error::DecodeError| e.to_string())?;
    Ok(value)
}

fn read_node<T: ReadableTable<&'static [u8; 16], &'static [u8]>>(
    table: &T,
    id: &NodeId,
) -> Result<Option<NodeRecord>, String> {
    let Some(guard) = table.get(id).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    decode(guard.value()).map(Some)
}

fn read_owned<T: Decode<()>>(table: &BytesTable<'_>, path: &str) -> Result<Option<T>, String> {
    let Some(guard) = table.get(path).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    decode(guard.value()).map(Some)
}

fn write_owned<T: Encode>(
    table: &mut BytesTable<'_>,
    path: &str,
    record: &T,
) -> Result<(), String> {
    let bytes = encode(record)?;
    table
        .insert(path, bytes.as_slice())
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Fold `incoming` into `existing`: declaration sites accumulate.
///
/// A package name already known is not cleared by a file that declares
/// none. The narrow limitation this leaves: a file that *drops* its package
/// clause while a sibling still declares the package leaves the old name in
/// place until the sibling is re-scanned. That input is not valid Go, and
/// the fix is to store a name per site rather than to guess.
fn merge_node(existing: NodeRecord, incoming: NodeRecord) -> NodeRecord {
    let known_name = match &existing {
        NodeRecord::Package { name, .. } => name.clone(),
        _ => None,
    };
    let mut sites = existing.into_declarations();
    let mut merged = incoming;
    if let NodeRecord::Package { name, .. } = &mut merged
        && name.is_none()
    {
        *name = known_name;
    }
    sites.append(merged.declarations_mut());
    sites.sort();
    sites.dedup();
    *merged.declarations_mut() = sites;
    merged
}

fn upsert_node(table: &mut NodeTable<'_>, id: &NodeId, record: NodeRecord) -> Result<(), String> {
    let merged = match read_node(table, id)? {
        Some(existing) => merge_node(existing, record),
        None => {
            let mut fresh = record;
            let sites = fresh.declarations_mut();
            sites.sort();
            sites.dedup();
            fresh
        }
    };
    let bytes = encode(&merged)?;
    table
        .insert(id, bytes.as_slice())
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Drop one file's declaration of a node, and the node with it when nothing
/// declares it any more.
///
/// The conditional is the whole point: file-granular invalidation must not
/// delete a node another file still declares.
fn drop_site(table: &mut NodeTable<'_>, id: &NodeId, path: &str) -> Result<(), String> {
    let Some(mut record) = read_node(table, id)? else {
        return Ok(());
    };
    record.declarations_mut().retain(|site| site.file != path);
    if record.declarations().is_empty() {
        table.remove(id).map_err(|e| e.to_string())?;
    } else {
        let bytes = encode(&record)?;
        table
            .insert(id, bytes.as_slice())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Remove everything phase 2 owns for one file. The ownership record itself
/// is left for the caller: a replace rewrites it, a forget removes it.
#[allow(clippy::too_many_arguments)]
fn forget_ref_half(
    nodes: &mut NodeTable<'_>,
    refs: &mut RefTable<'_>,
    edges: &mut EdgeTable<'_>,
    rev: &mut EdgeTable<'_>,
    cands: &mut CandidateTable<'_>,
    owned: &BytesTable<'_>,
    path: &str,
) -> Result<(), String> {
    let Some(previous) = read_owned::<RefOwned>(owned, path)? else {
        return Ok(());
    };
    for encoded in &previous.rows {
        refs.remove((path, encoded.as_slice()))
            .map_err(|e| e.to_string())?;
    }
    for (cand, encoded) in &previous.candidates {
        cands
            .remove(cand, (path, encoded.as_slice()))
            .map_err(|e| e.to_string())?;
    }
    for (src, dst, kind) in &previous.edges {
        edges.remove((src, dst, *kind)).map_err(|e| e.to_string())?;
        rev.remove((dst, src, *kind)).map_err(|e| e.to_string())?;
    }
    for id in &previous.nodes {
        drop_site(nodes, id, path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Domain, Lang, node_id};

    fn open_temp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("graph.redb")).expect("open");
        (dir, store)
    }

    fn site(file: &str, line: u32) -> DeclSite {
        DeclSite {
            file: file.to_string(),
            line,
        }
    }

    fn key(file: &str, raw: &str) -> RefKey {
        RefKey {
            file: file.to_string(),
            kind: 0,
            space: 0,
            enclosing: "m/pkg.Caller".to_string(),
            raw_target: raw.to_string(),
            argc: None,
        }
    }

    #[test]
    fn a_batch_round_trips_through_both_halves() {
        let (_dir, store) = open_temp();
        let def = node_id(Domain::Go, "m/pkg.Foo");
        let caller = node_id(Domain::Go, "m/pkg.Bar");
        let defs = DefBatch {
            files: vec![FileDefs {
                path: "pkg/a.go".into(),
                nodes: vec![(
                    def,
                    NodeRecord::Definition {
                        fqn: "m/pkg.Foo".into(),
                        kind: 0,
                        declarations: vec![site("pkg/a.go", 3)],
                    },
                )],
            }],
        };
        assert!(store.apply_defs(&defs).expect("apply defs").is_empty());

        let row = key("pkg/b.go", "Foo");
        let refs = RefBatch {
            files: vec![FileRefs {
                path: "pkg/b.go".into(),
                hash: [7u8; 32],
                nodes: vec![],
                rows: vec![(
                    row.clone(),
                    RefRecord {
                        outcome: StoredOutcome::Resolved(def),
                        count: 4,
                        first_line: 9,
                        lang: Lang::Go.code(),
                    },
                )],
                edges: vec![(caller, def, 0)],
                candidates: vec![(def, row.clone())],
            }],
        };
        store.apply_refs(&refs).expect("apply refs");

        assert_eq!(store.file_hash("pkg/b.go").unwrap(), Some([7u8; 32]));
        assert_eq!(store.file_hash("missing.go").unwrap(), None);
        assert_eq!(store.known_files().unwrap(), vec!["pkg/b.go".to_string()]);
        assert!(store.symbol_entries().unwrap().contains_key(&def));
        assert_eq!(store.candidate_rows(&def).unwrap(), vec![row]);
        assert!(store.has_edge(&caller, &def, 0).unwrap());
    }

    #[test]
    fn report_sums_counts_by_language_and_reason() {
        let (_dir, store) = open_temp();
        let def = node_id(Domain::Go, "m/pkg.Foo");
        let rec = |outcome, count| RefRecord {
            outcome,
            count,
            first_line: 1,
            lang: Lang::Go.code(),
        };
        let refs = RefBatch {
            files: vec![
                FileRefs {
                    path: "a.go".into(),
                    hash: [0u8; 32],
                    rows: vec![
                        (key("a.go", "Foo"), rec(StoredOutcome::Resolved(def), 3)),
                        (key("a.go", "x.Close"), rec(StoredOutcome::Unresolved(5), 2)),
                    ],
                    ..FileRefs::default()
                },
                FileRefs {
                    path: "b.go".into(),
                    hash: [0u8; 32],
                    rows: vec![(
                        RefKey {
                            kind: 1,
                            ..key("b.go", "fmt")
                        },
                        rec(StoredOutcome::External("std:fmt".into()), 1),
                    )],
                    ..FileRefs::default()
                },
            ],
        };
        store.apply_refs(&refs).expect("apply refs");
        let report = store.report().expect("report");
        let go = &report.per_lang[&Lang::Go.code()];
        assert_eq!(go.resolved, 3);
        assert_eq!(go.external, 1);
        assert_eq!(go.unresolved_total(), 2);
        assert_eq!(go.unresolved[&5], 2);
        assert_eq!(report.fqn_collisions, 0);
        assert_eq!(
            crate::resolution_rate(go.resolved, go.unresolved_total()),
            Some(0.6)
        );
    }

    #[test]
    fn a_definition_two_files_declare_is_a_collision_but_a_package_is_not() {
        let (_dir, store) = open_temp();
        let twin = node_id(Domain::Go, "m/pkg.plat");
        let pkg = node_id(Domain::Go, "m/pkg");
        let declare = |path: &str, line: u32| FileDefs {
            path: path.to_string(),
            nodes: vec![
                (
                    pkg,
                    NodeRecord::Package {
                        import_path: "m/pkg".into(),
                        name: Some("pkg".into()),
                        declarations: vec![site(path, 1)],
                    },
                ),
                (
                    twin,
                    NodeRecord::Definition {
                        fqn: "m/pkg.plat".into(),
                        kind: 0,
                        declarations: vec![site(path, line)],
                    },
                ),
            ],
        };
        let batch = DefBatch {
            files: vec![declare("pkg/a_linux.go", 3), declare("pkg/a_darwin.go", 4)],
        };
        let colliding = store.apply_defs(&batch).expect("apply defs");
        assert_eq!(colliding, vec![twin], "only the definition collides");
        assert_eq!(store.report().unwrap().fqn_collisions, 1);
        // Both attributions survive: neither declaration overwrote the other.
        let record = store.node(&twin).unwrap().expect("the twin node");
        assert_eq!(
            record.declarations(),
            [site("pkg/a_darwin.go", 4), site("pkg/a_linux.go", 3)],
            "declaration sites are sorted, so a snapshot is order-free"
        );
    }
}
