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

use crate::UnresolvedReason;
use crate::lang::Entry;
use crate::model::{DefFacets, DefKind, Lang, NodeId, reason_code};

/// On-disk schema generation.
///
/// A store written under any other value is dropped and rebuilt rather than
/// migrated: a graph is a cache of facts that can always be recomputed from
/// the source tree, and a half-migrated one is worse than an absent one.
pub const SCHEMA_VERSION: u32 = 7;

/// The [`META`] key the schema generation is stored under.
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// The [`META`] key the resolver's manifest fingerprint is stored under.
const CONFIG_DIGEST_KEY: &str = "config_digest";

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const NODES: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("nodes");
const REFS: TableDefinition<(&str, &[u8]), &[u8]> = TableDefinition::new("refs");
/// One edge, as the tables key it: `(src, dst, `[`crate::model::RefKind`]` code)`.
type EdgeKey<'a> = (&'a [u8; 16], &'a [u8; 16], u8);

/// Edge → the files that produce it, sorted.
///
/// An edge is a *shared* fact, exactly as a node is: two files of one
/// package whose package-level references reach the same target produce the
/// same triple, and a third file may produce it again tomorrow. Storing the
/// producers rather than a unit is what stops one file being re-scanned or
/// deleted from taking another file's edge with it — the never-drop rule,
/// applied where the key is not per-file.
const EDGES: TableDefinition<EdgeKey<'static>, &[u8]> = TableDefinition::new("edges");
const REV_EDGES: TableDefinition<EdgeKey<'static>, &[u8]> = TableDefinition::new("rev_edges");
const CANDIDATES: MultimapTableDefinition<&[u8; 16], (&str, &[u8])> =
    MultimapTableDefinition::new("candidates");
const FILES: TableDefinition<&str, &[u8; 32]> = TableDefinition::new("files");
const DEF_OWNED: TableDefinition<&str, &[u8]> = TableDefinition::new("def_owned");
const REF_OWNED: TableDefinition<&str, &[u8]> = TableDefinition::new("ref_owned");
/// File → the supertypes each type it declares was placed at.
///
/// Keyed by file, like every other half, because that is what makes it
/// replaceable: a file re-scanned states its hierarchy afresh, and a file
/// forgotten takes its rows with it. Two files declaring one identity — legal
/// under a source-set twin — both keep their row and the reader merges them.
const SUPERS: TableDefinition<&str, &[u8]> = TableDefinition::new("supers");

type NodeTable<'txn> = redb::Table<'txn, &'static [u8; 16], &'static [u8]>;
type RefTable<'txn> = redb::Table<'txn, (&'static str, &'static [u8]), &'static [u8]>;
type EdgeTable<'txn> = redb::Table<'txn, EdgeKey<'static>, &'static [u8]>;
type CandidateTable<'txn> =
    redb::MultimapTable<'txn, &'static [u8; 16], (&'static str, &'static [u8])>;
type BytesTable<'txn> = redb::Table<'txn, &'static str, &'static [u8]>;

/// Where a node is declared: one file, one line, and what that file said.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
pub struct DeclSite {
    /// Repo-relative path of the declaring file.
    pub file: String,
    /// 1-based line of the declaration.
    pub line: u32,
    /// What *this* file declared the node to be.
    ///
    /// A record carries one kind and one name; two files may declare one
    /// FQN and disagree — build-configuration-exclusive twins are legal Go
    /// and may declare `plat` as a func in one file and a type in the
    /// other. Keeping each file's answer beside its site is what lets the
    /// record be re-derived when a file is forgotten, instead of stranding
    /// the departing file's answer on the survivor.
    pub payload: NodePayload,
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
        /// What this identity forwards to, when it is an alias — a re-export,
        /// an export rename, a module-level import binding. Empty for every
        /// ordinary definition, and empty too for an alias key that stands
        /// for a *set* without forwarding, which is how Java spells an
        /// overload key.
        ///
        /// Re-derived from the declaration sites by [`resettle`], never
        /// written directly, so forgetting a file forgets its targets.
        targets: Vec<NodeId>,
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

/// The part of a [`NodeRecord`] a resolver's answer can depend on.
///
/// Declaration sites are deliberately absent: they move whenever a file is
/// edited above them, and nothing resolves against them, so folding them in
/// would wake every prober of every node in an edited file. What remains is
/// what makes an identity *mean* something different than it did — a
/// definition's kind, the name an unaliased import of a package binds — and
/// that has to wake the references that probed it even though the identity
/// itself never moved.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
pub enum NodePayload {
    /// A definition, by its [`crate::model::DefKind`] code.
    Definition(u8),
    /// A package, by the name its files declare — what an unaliased import
    /// of it binds.
    Package(Option<String>),
    /// A dependency outside the repository, by its package string.
    External(String),
    /// An alias, by its [`crate::model::DefKind`] code and what it forwards
    /// to.
    ///
    /// The targets ride the *site* and not only the record because
    /// [`resettle`] re-derives a record from the sites that survive: an alias
    /// whose declaring file is forgotten must lose its targets with it, and a
    /// record-only field would strand them. Changing where an alias points is
    /// also a change of meaning under a stable identity, which is exactly
    /// what this type exists to wake probers on.
    Alias(u8, Vec<NodeId>),
}

impl NodeRecord {
    /// The part of this record a resolver's answer can depend on.
    pub fn payload(&self) -> NodePayload {
        match self {
            NodeRecord::Definition { kind, targets, .. } if !targets.is_empty() => {
                NodePayload::Alias(*kind, targets.clone())
            }
            NodeRecord::Definition { kind, .. } => NodePayload::Definition(*kind),
            NodeRecord::Package { name, .. } => NodePayload::Package(name.clone()),
            NodeRecord::External { package, .. } => NodePayload::External(package.clone()),
        }
    }

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
///
/// A row carries exactly one outcome, so the key must separate every pair of
/// references whose outcomes can legitimately differ. Everything the resolver
/// reads is either in this key or derived from it — except the extractor's
/// binding verdict, which is why [`RefKey::locally_bound`] is part of it.
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
    /// Whether some enclosing block binds the target's root name at this
    /// site.
    ///
    /// Two calls can agree on every other field and still be different
    /// references: an inner block's `x()` and the package-level `x()` after
    /// it share a file, an enclosing function, a site text and an arity, and
    /// resolve to `LocalBinding` and `Resolved` respectively. Without this
    /// field they are one row, which keeps the first outcome and attributes
    /// both occurrences to it — every count still sums, and the rate is
    /// wrong in both terms.
    pub locally_bound: bool,
}

/// The non-`file` half of a [`RefKey`], as [`RefKey::split`] encodes it:
/// `(kind, space, enclosing, raw_target, argc, locally_bound)`.
type RefKeyRest = (u8, u8, String, String, Option<u32>, bool);

impl RefKey {
    /// Split into the redb key `(file, encoded rest)`.
    ///
    /// The file leads so that every row of one file is one contiguous range,
    /// which is what makes a per-file replace a bounded operation. The rest
    /// is bincode over
    /// `(kind, space, enclosing, raw_target, argc, locally_bound)` and is
    /// canonical: one key, one byte string.
    ///
    /// # Panics
    ///
    /// Never in practice: the encoded tuple is two bytes, two strings, an
    /// optional integer and a bool, and encoding those into a `Vec` cannot
    /// fail.
    pub fn split(&self) -> (&str, Vec<u8>) {
        let rest = (
            self.kind,
            self.space,
            self.enclosing.as_str(),
            self.raw_target.as_str(),
            self.argc,
            self.locally_bound,
        );
        let encoded = bincode::encode_to_vec(rest, config::standard())
            .expect("a row key encodes: two bytes, two strings, an optional integer and a bool");
        (self.file.as_str(), encoded)
    }

    /// Rebuild a key from the redb pair [`RefKey::split`] produced.
    ///
    /// Trailing bytes are an error rather than ignored padding: an encoding
    /// that accepts two byte strings for one key is not a key at all.
    pub fn join(file: &str, encoded: &[u8]) -> Result<RefKey, String> {
        let ((kind, space, enclosing, raw_target, argc, locally_bound), used): (RefKeyRest, usize) =
            bincode::decode_from_slice(encoded, config::standard()).map_err(|e| e.to_string())?;
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
            locally_bound,
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

/// One type's direct supertypes, as the supertype phase placed them.
///
/// The storage mirror of [`crate::lang::Supertypes`], and the reason it is a
/// record rather than a bare list: "declares nothing above it" and "declares
/// something this scan could not place" are different facts about the same
/// empty list, and a resolver reading them as one either invents a complete
/// closure or refuses to believe one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
pub struct SuperRecord {
    /// FQNs of the supertypes that resolved to a definition in this
    /// repository, in declaration order.
    pub supers: Vec<String>,
    /// Whether every supertype the type declares is in `supers`.
    pub complete: bool,
}

impl SuperRecord {
    /// Fold another file's row for the same identity into this one.
    ///
    /// Union rather than overwrite, and `complete` is an *and*: a source-set
    /// twin that names a base the other does not still names it, and a row
    /// that was short stays short however many files agree with the rest.
    pub fn merge(&mut self, other: SuperRecord) {
        for fqn in other.supers {
            if !self.supers.contains(&fqn) {
                self.supers.push(fqn);
            }
        }
        self.complete &= other.complete;
    }
}

/// Everything the supertype phase writes, one entry per file.
#[derive(Debug, Clone, Default)]
pub struct SuperBatch {
    /// One entry per file the event covers.
    pub files: Vec<FileSupers>,
}

/// One file's supertype half.
#[derive(Debug, Clone, Default)]
pub struct FileSupers {
    /// Repo-relative path.
    pub path: String,
    /// One row per type the file declares, by identity.
    pub types: Vec<(NodeId, SuperRecord)>,
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
    /// Occurrences whose target is bound by a local, parameter, named
    /// result or receiver.
    ///
    /// Policy-caused, not a language-support failure: locals are not nodes
    /// by design. Reported on its own line beside [`LangTally::external`]
    /// and excluded from **both** terms of the resolution rate. It never
    /// enters [`LangTally::unresolved`], so [`LangTally::unresolved_total`]
    /// excludes it structurally rather than by remembering to subtract.
    pub local_binding: u64,
    /// Occurrences unresolved, keyed by reason code. Never holds
    /// [`UnresolvedReason::LocalBinding`].
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
    /// The supertype half, by the file that declared it.
    pub supers: BTreeMap<String, Vec<(NodeId, SuperRecord)>>,
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
                drop_graph(&txn)?;
                let mut meta = txn.open_table(META).map_err(|e| e.to_string())?;
                meta.insert(SCHEMA_VERSION_KEY, &SCHEMA_VERSION.to_le_bytes()[..])
                    .map_err(|e| e.to_string())?;
            }
            create_graph(&txn)?;
        }
        txn.commit().map_err(|e| e.to_string())?;
        Ok(Store { db })
    }

    /// Fence the store on the resolver's manifest fingerprint, wiping it
    /// when the project it describes is no longer this one.
    ///
    /// A manifest is a scan input the walk never hashes: it carries no
    /// extension a language owns and contributes no facts of its own. It
    /// still decides every identity in the graph — a Go module path is the
    /// root of every FQN beneath it — so rewriting one renames every node
    /// while not a single source file's bytes move. The changed set comes
    /// out empty and the store keeps a graph no cold scan would ever build.
    ///
    /// Wiped rather than patched, for the same reason a schema change is: a
    /// graph is a cache of facts the source tree can always rebuild, and
    /// every identity in it is downstream of the fingerprint.
    ///
    /// The fence is **per language**: the fingerprint is stored under the
    /// language's own key, and going stale forgets only the files that
    /// language owns. Two live languages share one store, and one language's
    /// manifest says nothing about the identities of another's graph — a
    /// global fence made every second language's scan wipe the first's rows.
    /// An **empty digest is no opinion**: a language with no project
    /// manifest is never invalidated by this, exactly as
    /// [`crate::lang::Resolver::config_digest`] promises, and stores nothing.
    ///
    /// Returns whether the language's slice of the store was wiped.
    pub fn fence_config(&self, lang: Lang, digest: &[u8]) -> Result<bool, String> {
        if digest.is_empty() {
            return Ok(false);
        }
        let key = format!("{CONFIG_DIGEST_KEY}:{}", lang.name());
        let stale = {
            let txn = self.db.begin_read().map_err(|e| e.to_string())?;
            match txn.open_table(META) {
                Ok(meta) => {
                    let stored = meta
                        .get(key.as_str())
                        .map_err(|e| e.to_string())?
                        .map(|guard| guard.value().to_vec());
                    stored.as_deref() != Some(digest)
                }
                // A store no scan has written yet has no META table — and
                // nothing to wipe either way.
                Err(_) => true,
            }
        };
        if stale {
            let owned: Vec<String> = self
                .known_files()?
                .into_iter()
                .filter(|file| {
                    std::path::Path::new(file)
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|ext| lang.extensions().contains(&ext))
                })
                .collect();
            self.forget_files(&owned)?;
            // The digest is written last: a crash between the forget and
            // this write re-fences as stale next scan and forgets again,
            // which is idempotent.
            let txn = self.db.begin_write().map_err(|e| e.to_string())?;
            {
                let mut meta = txn.open_table(META).map_err(|e| e.to_string())?;
                meta.insert(key.as_str(), digest)
                    .map_err(|e| e.to_string())?;
            }
            txn.commit().map_err(|e| e.to_string())?;
        }
        Ok(stale)
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

    /// Replace the supertype half of every file in the batch, in one
    /// transaction.
    ///
    /// A file with no types to state is *removed* rather than written empty:
    /// most files declare no type at all, and a row per file would be a table
    /// the size of the tree carrying nothing. Removal is also what keeps a
    /// warm store byte-identical to a cold one, which the snapshot oracle
    /// compares.
    pub fn apply_supers(&self, batch: &SuperBatch) -> Result<(), String> {
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut supers = txn.open_table(SUPERS).map_err(|e| e.to_string())?;
            for file in &batch.files {
                if file.types.is_empty() {
                    supers
                        .remove(file.path.as_str())
                        .map_err(|e| e.to_string())?;
                    continue;
                }
                let mut rows = file.types.clone();
                rows.sort_by_key(|row| row.0);
                let bytes = encode(&rows)?;
                supers
                    .insert(file.path.as_str(), bytes.as_slice())
                    .map_err(|e| e.to_string())?;
            }
        }
        txn.commit().map_err(|e| e.to_string())
    }

    /// The whole supertype relation, merged across the files that state it.
    ///
    /// The map a resolver probes through [`crate::lang::SymbolProbe`]. Two
    /// files declaring one identity contribute both their supertypes, and the
    /// merged row is complete only if both were — the same conservatism the
    /// per-file record uses, applied where the rows meet.
    pub fn supertype_index(&self) -> Result<HashMap<NodeId, SuperRecord>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(SUPERS).map_err(|e| e.to_string())?;
        let mut out: HashMap<NodeId, SuperRecord> = HashMap::new();
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (_, value) = entry.map_err(|e| e.to_string())?;
            let rows: Vec<(NodeId, SuperRecord)> = decode(value.value())?;
            for (id, record) in rows {
                let merged = merge_supers(out.remove(&id), record);
                out.insert(id, merged);
            }
        }
        Ok(out)
    }

    /// The supertype rows these files state, merged, as they stand right now.
    ///
    /// Read before the supertype phase writes and again after, and compared:
    /// a type whose supertypes moved changes what every member lookup below
    /// it can reach, under an identity that never moved at all. That is the
    /// same invalidation [`Store::declared_nodes`] performs for a definition's
    /// payload, asked about the other half of what an identity means.
    pub fn declared_supers(
        &self,
        paths: &[String],
    ) -> Result<BTreeMap<NodeId, SuperRecord>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(SUPERS).map_err(|e| e.to_string())?;
        let mut out: BTreeMap<NodeId, SuperRecord> = BTreeMap::new();
        for path in paths {
            let Some(guard) = table.get(path.as_str()).map_err(|e| e.to_string())? else {
                continue;
            };
            let rows: Vec<(NodeId, SuperRecord)> = decode(guard.value())?;
            for (id, record) in rows {
                let merged = merge_supers(out.remove(&id), record);
                out.insert(id, merged);
            }
        }
        Ok(out)
    }

    /// Every definition's canonical name, by identity.
    ///
    /// The supertype phase resolves a base-class reference to a `NodeId` and
    /// has to write down a *name*: a member key is built from the owning
    /// type's FQN, and a 128-bit hash cannot be turned back into one. Only
    /// definitions are here — a base that placed at a package or an external
    /// node is not a type this graph can walk into, and leaving it out is what
    /// makes the row's `complete` flag false rather than a dangling name.
    pub fn definition_fqns(&self) -> Result<HashMap<NodeId, String>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(NODES).map_err(|e| e.to_string())?;
        let mut out = HashMap::new();
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            if let NodeRecord::Definition { fqn, .. } = decode(value.value())? {
                out.insert(*key.value(), fqn);
            }
        }
        Ok(out)
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
                    claim_edge(&mut edges, (src, dst, *kind), &file.path)?;
                    claim_edge(&mut rev, (dst, src, *kind), &file.path)?;
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
            let mut supers = txn.open_table(SUPERS).map_err(|e| e.to_string())?;
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
                supers.remove(path.as_str()).map_err(|e| e.to_string())?;
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
    ///
    /// A definition carrying alias targets answers as an alias rather than as
    /// a definition, because that is what it *is*: the kind says only that a
    /// re-export was written, and the targets say what it names. One target
    /// is an [`Entry::Alias`], several an [`Entry::Set`], and none leaves the
    /// definition speaking for itself — which is how an alias key that
    /// forwards to nothing, such as Java's overload key, keeps its old
    /// answer.
    pub fn symbol_entries(&self) -> Result<HashMap<NodeId, Entry>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(NODES).map_err(|e| e.to_string())?;
        let mut out = HashMap::new();
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            let record: NodeRecord = decode(value.value())?;
            let typed = match record {
                // An alias forwarding to exactly one identity is an
                // `Entry::Alias`; to several, an `Entry::Set` — a star export
                // taken from four modules is a name set, not a single
                // forward, and the resolver has to see the difference to tell
                // a genuine ambiguity from a chain it can walk.
                NodeRecord::Definition { targets, .. } if targets.len() == 1 => {
                    Entry::Alias { target: targets[0] }
                }
                NodeRecord::Definition { targets, .. } if targets.len() > 1 => Entry::Set(targets),
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

    /// What phase 1 owns for these files, by identity and [`NodePayload`].
    ///
    /// Read *before* an event writes anything: it is the only record of what
    /// the event's own files declared beforehand, and comparing it with what
    /// they declare afterwards is what selects the unchanged files this
    /// event has to wake. Read it after [`Store::apply_defs`] and the
    /// comparison is against itself.
    ///
    /// Carries the payload and not just the identity, because an identity
    /// can stay put while its meaning moves: a package's node is its import
    /// path, which its *directory* decides, so rewriting a `package` clause
    /// changes no id at all — and changes what every unaliased import of it
    /// binds.
    pub fn declared_nodes(
        &self,
        paths: &[String],
    ) -> Result<BTreeMap<NodeId, NodePayload>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let owned_table = txn.open_table(DEF_OWNED).map_err(|e| e.to_string())?;
        let nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
        let mut out = BTreeMap::new();
        for path in paths {
            let Some(guard) = owned_table.get(path.as_str()).map_err(|e| e.to_string())? else {
                continue; // a file the store has no phase-1 half for
            };
            let owned: DefOwned = decode(guard.value())?;
            for id in owned.nodes {
                // Owned but absent is not a contradiction to resolve here:
                // there is simply no earlier meaning to compare against, and
                // the identity still counts as one this event declares.
                if let Some(record) = read_node(&nodes, &id)? {
                    out.insert(id, record.payload());
                }
            }
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

    /// Every reference row that probed any of these identities, deduplicated.
    ///
    /// [`Store::candidate_rows`] answers for one identity and opens one read
    /// transaction to do it; an event asks for every identity it created or
    /// destroyed at once, and a transaction per identity is the difference
    /// between a bounded read and thousands of them on a cold store.
    pub fn rows_for(&self, ids: &[NodeId]) -> Result<BTreeSet<RefKey>, String> {
        if ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn
            .open_multimap_table(CANDIDATES)
            .map_err(|e| e.to_string())?;
        let mut out = BTreeSet::new();
        for id in ids {
            for value in table.get(id).map_err(|e| e.to_string())? {
                let guard = value.map_err(|e| e.to_string())?;
                let (file, encoded) = guard.value();
                out.insert(RefKey::join(file, encoded)?);
            }
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
            supers: BTreeMap::new(),
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
        let supers = txn.open_table(SUPERS).map_err(|e| e.to_string())?;
        for entry in supers.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            snapshot
                .supers
                .insert(key.value().to_string(), decode(value.value())?);
        }
        Ok(snapshot)
    }

    /// Tally every reference row into per-language counts, and count the
    /// FQNs two files' definitions both claim.
    pub fn report(&self) -> Result<Report, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let mut report = Report::default();
        let local_binding = reason_code(&UnresolvedReason::LocalBinding);
        let refs = txn.open_table(REFS).map_err(|e| e.to_string())?;
        for entry in refs.iter().map_err(|e| e.to_string())? {
            let (_, value) = entry.map_err(|e| e.to_string())?;
            let record: RefRecord = decode(value.value())?;
            let tally = report.per_lang.entry(record.lang).or_default();
            match record.outcome {
                StoredOutcome::Resolved(_) => tally.resolved += u64::from(record.count),
                StoredOutcome::External(_) => tally.external += u64::from(record.count),
                // The wire keeps the three-variant contract intact:
                // `LocalBinding` rides inside `Unresolved(6)` and is split
                // out here, onto its own line, never into the reason map.
                StoredOutcome::Unresolved(reason) if reason == local_binding => {
                    tally.local_binding += u64::from(record.count);
                }
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

/// Drop every graph table. [`META`] is deliberately untouched: it carries
/// the generation and fingerprint that decided the drop.
fn drop_graph(txn: &redb::WriteTransaction) -> Result<(), String> {
    txn.delete_table(NODES).map_err(|e| e.to_string())?;
    txn.delete_table(REFS).map_err(|e| e.to_string())?;
    txn.delete_table(EDGES).map_err(|e| e.to_string())?;
    txn.delete_table(REV_EDGES).map_err(|e| e.to_string())?;
    txn.delete_multimap_table(CANDIDATES)
        .map_err(|e| e.to_string())?;
    txn.delete_table(FILES).map_err(|e| e.to_string())?;
    txn.delete_table(DEF_OWNED).map_err(|e| e.to_string())?;
    txn.delete_table(REF_OWNED).map_err(|e| e.to_string())?;
    txn.delete_table(SUPERS).map_err(|e| e.to_string())?;
    Ok(())
}

/// Create every graph table, so a later read transaction finds it.
fn create_graph(txn: &redb::WriteTransaction) -> Result<(), String> {
    txn.open_table(NODES).map_err(|e| e.to_string())?;
    txn.open_table(REFS).map_err(|e| e.to_string())?;
    txn.open_table(EDGES).map_err(|e| e.to_string())?;
    txn.open_table(REV_EDGES).map_err(|e| e.to_string())?;
    txn.open_multimap_table(CANDIDATES)
        .map_err(|e| e.to_string())?;
    txn.open_table(FILES).map_err(|e| e.to_string())?;
    txn.open_table(DEF_OWNED).map_err(|e| e.to_string())?;
    txn.open_table(REF_OWNED).map_err(|e| e.to_string())?;
    txn.open_table(SUPERS).map_err(|e| e.to_string())?;
    Ok(())
}

fn encode<T: Encode>(value: &T) -> Result<Vec<u8>, String> {
    bincode::encode_to_vec(value, config::standard()).map_err(|e| e.to_string())
}

fn decode<T: Decode<()>>(bytes: &[u8]) -> Result<T, String> {
    let (value, _) = bincode::decode_from_slice(bytes, config::standard())
        .map_err(|e: bincode::error::DecodeError| e.to_string())?;
    Ok(value)
}

/// Fold one file's supertype row into whatever another file already said.
fn merge_supers(existing: Option<SuperRecord>, incoming: SuperRecord) -> SuperRecord {
    let Some(mut held) = existing else {
        return incoming;
    };
    held.merge(incoming);
    held
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

/// Re-derive a record's own kind or name from its declaration sites.
///
/// A record carries one answer; its sites carry what each file actually
/// said. Taking the answer from the *first* site — sites are sorted by
/// `(file, line)` — makes it a function of the surviving set alone, which
/// is the property that matters: a store that forgot a file must hold what
/// a cold scan of what is left would build, and last-write-wins cannot,
/// because the last writer may be the file that just went.
///
/// A package takes the first site that *names* it: a file with no package
/// clause declares no name and must not erase the one its siblings declare.
fn resettle(record: &mut NodeRecord) {
    let sites = record.declarations().to_vec();
    match record {
        NodeRecord::Definition { kind, targets, .. } => {
            if let Some(k) = sites.iter().find_map(|s| match s.payload {
                NodePayload::Definition(k) => Some(k),
                NodePayload::Alias(k, _) => Some(k),
                _ => None,
            }) {
                *kind = k;
            }
            // Every surviving site's targets, in site order. A union and not
            // a first-wins pick: two files may legitimately declare one alias
            // key — that is what a star export re-exported from two places
            // is — and dropping either would make the walk miss a name the
            // corpus really does export.
            let mut merged: Vec<NodeId> = Vec::new();
            for site in &sites {
                if let NodePayload::Alias(_, ts) = &site.payload {
                    for t in ts {
                        if !merged.contains(t) {
                            merged.push(*t);
                        }
                    }
                }
            }
            *targets = merged;
        }
        NodeRecord::Package { name, .. } => {
            *name = sites.iter().find_map(|s| match &s.payload {
                NodePayload::Package(n) => n.clone(),
                _ => None,
            });
        }
        NodeRecord::External { package, .. } => {
            if let Some(p) = sites.iter().find_map(|s| match &s.payload {
                NodePayload::External(p) => Some(p.clone()),
                _ => None,
            }) {
                *package = p;
            }
        }
    }
}

/// Fold `incoming` into `existing`: declaration sites accumulate, and the
/// record's own answer is re-derived from the set they form.
fn merge_node(existing: NodeRecord, incoming: NodeRecord) -> NodeRecord {
    let mut sites = existing.into_declarations();
    let mut merged = incoming;
    sites.append(merged.declarations_mut());
    sites.sort();
    sites.dedup();
    *merged.declarations_mut() = sites;
    resettle(&mut merged);
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
            resettle(&mut fresh);
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
/// delete a node another file still declares. Neither may it leave the
/// departing file's answer behind — hence the [`resettle`], which re-derives
/// the record from the sites that remain.
fn drop_site(table: &mut NodeTable<'_>, id: &NodeId, path: &str) -> Result<(), String> {
    let Some(mut record) = read_node(table, id)? else {
        return Ok(());
    };
    record.declarations_mut().retain(|site| site.file != path);
    if record.declarations().is_empty() {
        table.remove(id).map_err(|e| e.to_string())?;
    } else {
        resettle(&mut record);
        let bytes = encode(&record)?;
        table
            .insert(id, bytes.as_slice())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The files that produce an edge, or an empty list when nothing does.
fn edge_producers(table: &EdgeTable<'_>, key: EdgeKey<'_>) -> Result<Vec<String>, String> {
    let Some(guard) = table.get(key).map_err(|e| e.to_string())? else {
        return Ok(Vec::new());
    };
    decode(guard.value())
}

/// Record that `path` produces this edge.
fn claim_edge(table: &mut EdgeTable<'_>, key: EdgeKey<'_>, path: &str) -> Result<(), String> {
    let mut producers = edge_producers(table, key)?;
    producers.push(path.to_string());
    producers.sort();
    producers.dedup();
    let bytes = encode(&producers)?;
    table
        .insert(key, bytes.as_slice())
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Drop one file's claim on an edge, and the edge with it when nothing
/// produces it any more.
///
/// The conditional is the whole point, and it is the same one
/// [`drop_site`] makes for nodes: an edge two files both produce must
/// survive either of them going. Deleting it unconditionally leaves a store
/// that is *nearly* right — the tallies never move, because they are counted
/// from per-file rows — and only a whole-store comparison against a cold
/// scan can see it.
fn release_edge(table: &mut EdgeTable<'_>, key: EdgeKey<'_>, path: &str) -> Result<(), String> {
    let mut producers = edge_producers(table, key)?;
    if producers.is_empty() {
        return Ok(());
    }
    producers.retain(|producer| producer != path);
    if producers.is_empty() {
        table.remove(key).map_err(|e| e.to_string())?;
    } else {
        let bytes = encode(&producers)?;
        table
            .insert(key, bytes.as_slice())
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
        release_edge(edges, (src, dst, *kind), path)?;
        release_edge(rev, (dst, src, *kind), path)?;
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
            payload: NodePayload::Definition(0),
        }
    }

    fn key(file: &str, raw: &str) -> RefKey {
        RefKey {
            file: file.to_string(),
            kind: 0,
            space: 0,
            enclosing: "m/pkg#Caller".to_string(),
            raw_target: raw.to_string(),
            argc: None,
            locally_bound: false,
        }
    }

    #[test]
    fn a_batch_round_trips_through_both_halves() {
        let (_dir, store) = open_temp();
        let def = node_id(Domain::Go, "m/pkg#Foo");
        let caller = node_id(Domain::Go, "m/pkg#Bar");
        let defs = DefBatch {
            files: vec![FileDefs {
                path: "pkg/a.go".into(),
                nodes: vec![(
                    def,
                    NodeRecord::Definition {
                        fqn: "m/pkg#Foo".into(),
                        kind: 0,
                        targets: Vec::new(),
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
        let def = node_id(Domain::Go, "m/pkg#Foo");
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
    fn a_local_binding_is_reported_beside_external_not_inside_unresolved() {
        // Structural exclusion, not arithmetic: code 6 never enters the
        // reason map, so `unresolved_total` cannot accidentally include it
        // and the rate cannot be gamed by growing the bucket.
        let (_dir, store) = open_temp();
        let def = node_id(Domain::Go, "m/pkg#Foo");
        let rec = |outcome, count| RefRecord {
            outcome,
            count,
            first_line: 1,
            lang: Lang::Go.code(),
        };
        let local = reason_code(&UnresolvedReason::LocalBinding);
        let refs = RefBatch {
            files: vec![FileRefs {
                path: "a.go".into(),
                hash: [0u8; 32],
                rows: vec![
                    (key("a.go", "Foo"), rec(StoredOutcome::Resolved(def), 1)),
                    (key("a.go", "missing"), rec(StoredOutcome::Unresolved(4), 1)),
                    (key("a.go", "cb"), rec(StoredOutcome::Unresolved(local), 3)),
                ],
                ..FileRefs::default()
            }],
        };
        store.apply_refs(&refs).expect("apply refs");
        let go = &store.report().expect("report").per_lang[&Lang::Go.code()];
        assert_eq!(go.local_binding, 3);
        assert_eq!(go.unresolved_total(), 1);
        assert!(!go.unresolved.contains_key(&local));
        assert_eq!(
            crate::resolution_rate(go.resolved, go.unresolved_total()),
            Some(0.5),
            "three local bindings leave both terms of the rate alone",
        );
    }

    #[test]
    fn a_definition_two_files_declare_is_a_collision_but_a_package_is_not() {
        let (_dir, store) = open_temp();
        let twin = node_id(Domain::Go, "m/pkg#plat");
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
                        fqn: "m/pkg#plat".into(),
                        kind: 0,
                        targets: Vec::new(),
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
