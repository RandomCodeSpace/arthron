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
//! Ownership carries a second obligation, and it is what makes an
//! interrupted scan survivable: **a file may be recorded current only once
//! every one of its facts is committed.** A phase writing half a file's facts
//! withdraws the store's currency claim in the same transaction, and
//! [`Store::apply_refs`] — the last half — is the only thing that gives it
//! back. The same transaction withdraws the claim of every *other* file whose
//! stored resolution consulted an identity it moved, because the candidate
//! index says exactly which those are. So a process killed between two
//! commits leaves a store that knows what it no longer knows: the next scan
//! re-reads precisely the files whose answers this one falsified, and lands
//! on the graph a cold scan of the same tree builds. Nothing else recovers
//! it — a content hash that outlives the facts it vouches for is a file no
//! later scan has any reason to look at again.
//!
//! One write transaction per batch, and a batch is a fixed number of files
//! rather than the whole event (500 files in one transaction measured 60ms
//! against 216ms as 500 separate transactions). What the bound buys is this
//! layer's term and only this layer's: redb holds a transaction's dirty pages
//! until it commits, so an event-sized transaction is an event-sized
//! allocation, and a batch boundary hands both the batch and those pages
//! back. It does not bound the scan — the driver holds the whole changed set
//! while these run, and that term is the corpus's. See
//! [`crate::pipeline::scan`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use bincode::{Decode, Encode, config};
use redb::{
    Database, MultimapTableDefinition, ReadOnlyDatabase, ReadableDatabase, ReadableMultimapTable,
    ReadableTable, ReadableTableMetadata, TableDefinition,
};

use crate::UnresolvedReason;
use crate::lang::Entry;
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Lang, NodeId, Params, Span, reason_code,
};

/// On-disk schema generation.
///
/// A store written under any other value is dropped and rebuilt rather than
/// migrated: a graph is a cache of facts that can always be recomputed from
/// the source tree, and a half-migrated one is worse than an absent one.
pub const SCHEMA_VERSION: u32 = 10;

/// The [`META`] key the schema generation is stored under.
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// Bytes redb may hold in its page cache, read and dirty pages together.
///
/// redb's own default is 1 GiB, which is not a cache at all for a store this
/// size: a 257 MB graph fits inside it whole, nothing is ever evicted, and
/// the process's peak becomes the database's size. Naming a smaller budget
/// makes redb flush dirty pages as a transaction runs and evict clean ones as
/// it reads.
///
/// This caps redb's term, and the pipeline's batch bound holds the same term
/// down from the other side — a transaction over 500 files never has
/// much dirty at once — so the two overlap rather than add. Peak RSS on the
/// 1.79M-line reference corpus, 2 vCPU: 343 MB as shipped, 345 MB with
/// redb's 1 GiB default cache, 349 MB with one transaction per phase, 440 MB
/// with neither. The cap is kept because it is what holds the term when a
/// phase's transaction is large, and neither knob makes the scan's own
/// memory anything but linear in the tree — see [`crate::pipeline::scan`].
///
/// Chosen against the 512 MB ceiling on the 2 vCPU reference hardware, with
/// room left for the batch and the symbol table beside it. Correctness does
/// not depend on the number — a cache is a cache — only the ceiling does.
const CACHE_BYTES: usize = 96 * 1024 * 1024;

/// The [`META`] key the resolver's manifest fingerprint is stored under.
const CONFIG_DIGEST_KEY: &str = "config_digest";

/// What both open paths say when redb's exclusive lock is already taken.
///
/// redb's own words for it are `Database already open. Cannot acquire lock.`
/// — true, and it names neither the file nor the thing that is holding it, so
/// a person who ran two scans at once reads it as corruption. One sentence,
/// shared by [`Store::open`] and [`ReadStore::open`], because the situation
/// is the same one from either side: somebody else has the writer's lock.
///
/// The lock is `flock(2)`, which redb takes with `try_lock` — non-blocking.
/// A second open therefore *fails*; it does not queue behind the first and it
/// does not wait for a transaction to finish. That is the property worth
/// keeping, and `tests/store_held.rs` bounds it in wall-clock time so that a
/// future change to a blocking lock fails the build instead of hanging a
/// scan. `flock` conflicts are per open file description, so a second open
/// inside one process is refused exactly as a second process is — which is
/// why the sentence says *handle* rather than *process*: it is true of both,
/// and naming the process would send a reader who opened the store twice
/// hunting for a second one that does not exist.
pub const HELD_FOR_WRITING: &str =
    "the store is held open for writing by another handle — a scan is already running against it";

/// What both open paths say about bytes that are not a graph.
///
/// redb makes a database in two steps: it sizes the file and syncs it, and
/// only then writes the magic number that says the bytes are a database —
/// deliberately, so that a half-made file can never be mistaken for a whole
/// one. The cost of that safety is that the window has no exit: a process
/// killed inside it leaves a file every later open refuses, with no repair
/// path and nothing in redb's own words (`I/O error: invalid data`) to say
/// which file or what to do about it.
///
/// [`Store::open`] does not leave one any more — a store that does not exist
/// yet is built beside its path and published whole, so the path holds either
/// nothing or a database. This sentence is for the two cases that remain: a
/// file an older build wedged, and a `--db` aimed at something that was never
/// a store. Both read the same from here, and both end the same way — and not
/// by arthron's hand. A graph is a cache the tree can always rebuild, but the
/// bytes at a path the caller named are the caller's to delete.
pub const NOT_A_STORE: &str = "not an arthron store — an interrupted first scan left it \
     half-made, or the path is not a graph at all; delete the file and re-run `arthron scan`";

/// What a read-only open says about a store an interrupted scan left behind.
///
/// redb marks a store as needing recovery while a write transaction is in
/// flight and clears the mark when it lands, so a scan killed between the two
/// leaves the mark set. Recovery is a *write*, which a reader cannot do and
/// must not: repairing a store in order to answer a question is the one thing
/// [`ReadStore`] exists not to do. redb's own words — `Database repair
/// aborted.` — name neither the store nor the way out.
///
/// The refusal is wider than the damage, on purpose. The mark says a scan
/// died; it does not say the graph is wrong, and a store can carry it while
/// being byte-for-byte what the finished scan would have written. Nothing in
/// the file distinguishes those two, so answering out of one would mean
/// answering out of the ones that *are* torn — and the way out is one command
/// that fixes both, because a scan re-reads every file whose claim the killed
/// one withdrew.
pub const NEEDS_RECOVERY: &str =
    "an interrupted scan left this store needing recovery — re-run `arthron scan`";

/// What a query says about a store that is readable but not wholly current.
///
/// A [`FILES`] row with an empty value is the store saying it holds facts for
/// a file it no longer claims are current — see [`Store::forget_hashes`] and
/// the currency rule in this module's header. Two things leave one: a scan
/// that could not read an owned file, and a scan that was killed between a
/// file's two halves. A query cannot tell them apart and does not need to,
/// because the consequence is the same either way — some of these answers
/// were computed against a graph the store has since moved past.
///
/// Said on stderr and never on stdout: the answer is still the best one the
/// store has, `--json` stays one document, and the exit code stays what the
/// question deserved. What is not acceptable is saying it silently.
pub const NOT_ALL_CURRENT: &str = "this store no longer claims to be current for some of its files; \
     answers touching them may be stale — re-run `arthron scan`";

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const NODES: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("nodes");
const COLLISION_DISPOSITIONS: TableDefinition<&[u8; 16], u8> =
    TableDefinition::new("collision_dispositions");
// A missing disposition still has the direct-Store mechanical meaning of
// Collision. This table distinguishes that fresh, unclassified state from a
// verdict that a declaration-set change explicitly invalidated.
const COLLISION_VERDICTS_INVALIDATED: TableDefinition<&[u8; 16], u8> =
    TableDefinition::new("collision_verdicts_invalidated");
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
/// File → the content hash its stored facts were computed from, or an empty
/// value when the store makes no such claim.
///
/// The key set is the file set: a file the store holds facts for has a row
/// here, which is how a walk that no longer reaches it is read as a deletion.
/// The *value* is a separate statement — "these facts are current for these
/// bytes" — and a scan that could not read an owned file has to take that
/// statement back without taking the facts, so the row stays and the hash
/// goes. See [`Store::forget_hashes`].
const FILES: TableDefinition<&str, &[u8]> = TableDefinition::new("files");
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
    /// The declaration facts a language's [`crate::lang::Resolver::mergeable`]
    /// implementation may inspect.
    ///
    /// Present only for definition nodes. Keeping them per site lets the
    /// pipeline ask about every surviving pair after an incremental replace;
    /// an event-local vector cannot answer for declarations written by an
    /// earlier scan.
    pub merge_definition: Option<StoredDefinition>,
}

/// Serializable mirror of [`Definition`] for durable collision disposition.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
pub struct StoredDefinition {
    kind: u8,
    name: String,
    owner: Vec<String>,
    space: u8,
    facets: u16,
    params: Option<StoredParams>,
    byte_start: u32,
    byte_end: u32,
    line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct StoredParams {
    count: u32,
    varargs: bool,
    types: Vec<String>,
}

impl StoredDefinition {
    /// Capture every field a resolver may read when comparing declarations.
    pub fn from_definition(def: &Definition) -> StoredDefinition {
        StoredDefinition {
            kind: def.kind.code(),
            name: def.name.clone(),
            owner: def.owner.clone(),
            space: def.space.code(),
            facets: def.facets.bits(),
            params: def.params.as_ref().map(|params| StoredParams {
                count: params.count,
                varargs: params.varargs,
                types: params.types.clone(),
            }),
            byte_start: def.span.byte_start,
            byte_end: def.span.byte_end,
            line: def.span.line,
        }
    }

    fn definition(&self) -> Result<Definition, String> {
        Ok(Definition {
            kind: DefKind::from_code(self.kind)
                .ok_or_else(|| format!("stored definition kind {} is invalid", self.kind))?,
            name: self.name.clone(),
            owner: self.owner.clone(),
            space: DeclSpace::from_code(self.space)
                .ok_or_else(|| format!("stored declaration space {} is invalid", self.space))?,
            facets: DefFacets::from_bits(self.facets),
            params: self.params.as_ref().map(|params| Params {
                count: params.count,
                varargs: params.varargs,
                types: params.types.clone(),
            }),
            span: Span {
                byte_start: self.byte_start,
                byte_end: self.byte_end,
                line: self.line,
            },
        })
    }
}

/// The language's durable verdict for a multi-file definition identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub enum CollisionDisposition {
    /// At least one declaration pair describes different entities.
    Collision,
    /// Every unordered declaration pair describes one entity.
    Mergeable,
}

impl CollisionDisposition {
    fn code(self) -> u8 {
        match self {
            CollisionDisposition::Collision => 0,
            CollisionDisposition::Mergeable => 1,
        }
    }

    fn from_code(code: u8) -> Result<CollisionDisposition, String> {
        match code {
            0 => Ok(CollisionDisposition::Collision),
            1 => Ok(CollisionDisposition::Mergeable),
            _ => Err(format!("stored collision disposition {code} is invalid")),
        }
    }
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
        /// [`crate::model::DefFacets`] bits: what the declaration *is* beyond
        /// what a reference can do with it.
        ///
        /// Stored because a resolver cannot branch on a fact the graph does
        /// not give back, and a resolver that has to infer one — "the
        /// constructor lookup missed, so the supertype must have been an
        /// interface" — is making a statement about its own search rather
        /// than about the declaration.
        ///
        /// Re-derived from the declaration sites by [`resettle`] alongside
        /// `kind`, and from the *same* site: two files may declare one FQN
        /// and disagree, and a record holding one file's kind beside
        /// another's facets would describe a declaration nobody wrote.
        facets: u16,
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
    /// A definition, by its [`crate::model::DefKind`] code and its
    /// [`crate::model::DefFacets`] bits.
    ///
    /// The facets are here and not only on the record because this type is
    /// what wakes probers: `class T` rewritten to `interface T` moves no
    /// identity at all — the FQN is the same — and a resolver that branched
    /// on the facet answered a question that just changed. A facet a
    /// resolver may read and the invalidation index may not see is a warm
    /// scan that disagrees with a cold one.
    Definition(u8, u16),
    /// A package, by the name its files declare — what an unaliased import
    /// of it binds.
    Package(Option<String>),
    /// A dependency outside the repository, by its package string.
    External(String),
    /// An alias, by its [`crate::model::DefKind`] code, its
    /// [`crate::model::DefFacets`] bits, and what it forwards to.
    ///
    /// The targets ride the *site* and not only the record because
    /// [`resettle`] re-derives a record from the sites that survive: an alias
    /// whose declaring file is forgotten must lose its targets with it, and a
    /// record-only field would strand them. Changing where an alias points is
    /// also a change of meaning under a stable identity, which is exactly
    /// what this type exists to wake probers on.
    Alias(u8, u16, Vec<NodeId>),
}

impl NodeRecord {
    /// The part of this record a resolver's answer can depend on.
    pub fn payload(&self) -> NodePayload {
        match self {
            NodeRecord::Definition {
                kind,
                facets,
                targets,
                ..
            } if !targets.is_empty() => NodePayload::Alias(*kind, *facets, targets.clone()),
            NodeRecord::Definition { kind, facets, .. } => NodePayload::Definition(*kind, *facets),
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
    fn is_multi_file_definition(&self) -> bool {
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

/// What one phase-1 transaction leaves for the event that ran it.
///
/// Two answers, because a batch changes two things at once: which identities
/// exist, and which files' stored answers about them are still worth
/// anything.
#[derive(Debug, Clone, Default)]
pub struct DefOutcome {
    /// The identities that ended this call with a *definition* declared in
    /// more than one file — see [`Store::apply_defs`].
    pub colliding: Vec<NodeId>,
    /// The files whose currency claim this call withdrew, because an
    /// identity their stored resolution consulted moved underneath them.
    ///
    /// The event has to re-resolve every one of them before it ends: the
    /// claim is withdrawn *inside* the transaction that invalidates the
    /// file, so that a scan killed here leaves a store that knows what it no
    /// longer knows, and [`Store::apply_refs`] is the only thing that puts
    /// the claim back.
    pub invalidated: BTreeSet<String>,
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

/// One file a scan reached and could not turn into facts.
///
/// The never-drop rule, applied one level below a reference: a file that
/// cannot be read produces no reference at all, so no [`crate::Outcome`] can
/// carry the failure and a rate computed over the rest is silently taken over
/// a smaller file set than the one the walk found. Recording the file, with
/// what the filesystem or the decoder said, is what keeps that visible.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileError {
    /// The path, repo-relative when the walk got far enough to have one and
    /// as the walk saw it otherwise.
    pub path: String,
    /// One line: what failed.
    pub message: String,
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
    /// The files this scan could not read, sorted by path and deduplicated.
    ///
    /// Data like [`Report::fqn_collisions`], and filled by the same hand: the
    /// store tallies what it holds, and the walk that found these tells the
    /// report about them afterwards — [`Store::report`] leaves the list
    /// empty because a store cannot know what a walk failed to reach. A read
    /// failure is not a scan failure: the walk keeps going, every other file
    /// is measured, and the ones that were not are named here rather than
    /// vanishing between the file count and the reference count.
    ///
    /// Bounded by the walk's own file list, which a scan already holds in
    /// full, so carrying every failure costs nothing a scan was not already
    /// paying.
    pub file_errors: Vec<FileError>,
}

/// The whole store as one comparable value: the incremental oracle.
///
/// A report can agree while the graph underneath disagrees — a dangling
/// candidate entry or a node one file too many declares changes no tally.
/// This is what an incremental scan is compared against a cold one with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Content hash per known file, or `None` for one whose facts the store
    /// no longer claims are current — see [`Store::forget_hashes`].
    pub files: BTreeMap<String, Option<[u8; 32]>>,
    /// Every node, by identity.
    pub nodes: BTreeMap<NodeId, NodeRecord>,
    /// Durable language verdict for every classified multi-file definition.
    pub collision_dispositions: BTreeMap<NodeId, CollisionDisposition>,
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
    ///
    /// A store some other handle already holds is refused, immediately and by
    /// name — see [`HELD_FOR_WRITING`]. There is one writer, and a second one
    /// is told so rather than left waiting for the first to finish.
    ///
    /// # Creation is all-or-nothing
    ///
    /// A store that does not exist yet — or a path holding no bytes at all,
    /// which is the same thing to redb — is **not** made at its own path. It
    /// is made beside it, at [`staging_path`], and published only once it is a
    /// database — because the window in which it is not one has no exit. redb
    /// sizes the file and syncs it before writing the magic number, so a
    /// process killed between those two leaves bytes that every later open
    /// refuses, forever, with no repair path: the first scan of a repository
    /// wedges its own store and only `rm` gets past it. Building beside the
    /// path and publishing whole means the path holds either nothing — which
    /// the next scan reads as "cold scan" and heals by definition — or a
    /// database. See [`NOT_A_STORE`].
    ///
    /// Publication is a [`fs::hard_link`], which refuses to replace a path
    /// that exists, and *that* refusal is the point: two scans that both found
    /// no store both build one, and the loser must not unlink the winner's
    /// store out from under a handle already writing into it. The loser drops
    /// its own and opens the winner's, where redb's lock refuses it by name
    /// exactly as it would have before.
    pub fn open(path: &Path) -> Result<Self, String> {
        if !holds_bytes(path) {
            create_beside(path)?;
        }
        let db = redb::Builder::new()
            .set_cache_size(CACHE_BYTES)
            .create(path)
            .map_err(|e| open_failure(path, e))?;
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
    /// Returns the identities that ended *this call* with a *definition*
    /// declared in more than one file — the mechanical half of the FQN
    /// grammar's injectivity obligation. The store never judges what that
    /// means: two declarations sharing an FQN are a collision in one
    /// language and one entity in another, and only the language knows.
    ///
    /// "This call" is the whole of the promise. An event applies several
    /// batches, and a batch sees only the graph as it stands when it commits
    /// — an identity two files claim is flagged by whichever batch brings the
    /// second one in, and an identity a later batch takes back is flagged all
    /// the same. A caller spanning batches therefore collects these and asks
    /// [`Store::definition_collisions`] which of them survived the event.
    ///
    /// # Currency
    ///
    /// This writes half of a file's facts, so the same transaction
    /// **withdraws the store's claim that the file is current**: the other
    /// half is still the previous event's, and [`Store::apply_refs`] is what
    /// puts the claim back once it lands. A scan killed between the two
    /// therefore leaves a file the next scan re-reads, instead of a file
    /// claiming to be current for work that never finished.
    ///
    /// The same transaction withdraws the claim of every *other* file whose
    /// stored resolution consulted an identity this batch moved. Phase 1 is
    /// where a definition appears, disappears, or changes meaning, and every
    /// row that probed it — hit or miss — holds an answer that no longer
    /// stands. The event widens to those files itself, but the widening lives
    /// in memory and the commit does not: recording the invalidation in the
    /// transaction that *causes* it is what makes an interrupted scan
    /// recoverable rather than silently wrong. They are named back to the
    /// caller so that the running event re-resolves them and restores their
    /// claims.
    pub fn apply_defs(&self, batch: &DefBatch) -> Result<DefOutcome, String> {
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        let mut colliding = Vec::new();
        let invalidated;
        {
            let mut nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
            let mut dispositions = txn
                .open_table(COLLISION_DISPOSITIONS)
                .map_err(|e| e.to_string())?;
            let mut invalidated_verdicts = txn
                .open_table(COLLISION_VERDICTS_INVALIDATED)
                .map_err(|e| e.to_string())?;
            let mut owned = txn.open_table(DEF_OWNED).map_err(|e| e.to_string())?;
            let mut files = txn.open_table(FILES).map_err(|e| e.to_string())?;
            let cands = txn
                .open_multimap_table(CANDIDATES)
                .map_err(|e| e.to_string())?;
            // What every identity this transaction touches meant *before* it
            // ran, captured at the first write to each. Comparing that with
            // what it means at the end separates an identity whose meaning
            // moved from one a file merely re-asserted: a woken file rewrites
            // its own half unchanged, and invalidating its probers for that
            // would cascade an event across a repository.
            let mut before: BTreeMap<NodeId, Option<NodePayload>> = BTreeMap::new();
            // A collision disposition describes the whole declaration set,
            // not merely the payload the resolver probes. Keep that whole
            // record too: changing one file's site can leave the payload
            // alone while changing which declaration pairs need a verdict.
            let mut declarations_before: BTreeMap<NodeId, Option<NodeRecord>> = BTreeMap::new();
            for file in &batch.files {
                let previous: DefOwned = read_owned(&owned, &file.path)?.unwrap_or_default();
                for id in &previous.nodes {
                    note_payload(&nodes, &mut before, id)?;
                    note_node(&nodes, &mut declarations_before, id)?;
                    drop_site(&mut nodes, id, &file.path)?;
                }
                let mut ids = Vec::with_capacity(file.nodes.len());
                let mut seen: HashSet<NodeId> = HashSet::with_capacity(file.nodes.len());
                for (id, record) in &file.nodes {
                    note_payload(&nodes, &mut before, id)?;
                    note_node(&nodes, &mut declarations_before, id)?;
                    upsert_node(&mut nodes, id, record.clone())?;
                    if seen.insert(*id) {
                        ids.push(*id);
                    }
                }
                write_owned(&mut owned, &file.path, &DefOwned { nodes: ids })?;
                // Half of this file's facts are now this event's and the
                // other half is still the last one's. The store may not claim
                // the file is current until `apply_refs` lands the rest.
                withdraw_claim(&mut files, &file.path)?;
            }
            let mut moved: BTreeSet<NodeId> = BTreeSet::new();
            for (id, was) in &before {
                let record = read_node(&nodes, id)?;
                if record
                    .as_ref()
                    .is_some_and(NodeRecord::is_multi_file_definition)
                {
                    colliding.push(*id);
                }
                if record.map(|record| record.payload()).as_ref() != was.as_ref() {
                    moved.insert(*id);
                }
            }
            let rewritten: HashSet<&str> =
                batch.files.iter().map(|file| file.path.as_str()).collect();
            let mut changed = invalidate_probers(&cands, &mut files, &moved, &rewritten)?;
            for (id, was) in declarations_before {
                let now = read_node(&nodes, &id)?;
                if now == was {
                    continue;
                }
                // No resolver verdict survives a declaration-set change.
                // Removing it is not calling the set Mergeable: it is making
                // the store say no verdict until the resolver writes one.
                let had_verdict = dispositions
                    .remove(&id)
                    .map_err(|e| e.to_string())?
                    .is_some();
                if had_verdict
                    || invalidated_verdicts
                        .get(&id)
                        .map_err(|e| e.to_string())?
                        .is_some()
                {
                    invalidated_verdicts
                        .insert(&id, 1)
                        .map_err(|e| e.to_string())?;
                }
                let Some(record) = now else {
                    continue;
                };
                if !record.is_multi_file_definition() {
                    continue;
                }
                // The unchanged declarations contribute to the new set too.
                // Their old answers and the old disposition are one fact, so
                // they lose currency in the same commit that changes it.
                for site in record.declarations() {
                    if rewritten.contains(site.file.as_str()) {
                        continue;
                    }
                    withdraw_known(&mut files, &site.file)?;
                    changed.insert(site.file.clone());
                }
            }
            invalidated = changed;
        }
        txn.commit().map_err(|e| e.to_string())?;
        Ok(DefOutcome {
            colliding,
            invalidated,
        })
    }

    /// Which of these identities a *definition* is declared for in more than
    /// one file, as the graph stands right now.
    ///
    /// The settled answer to the question [`Store::apply_defs`] can only
    /// answer per batch. Every identity any batch flagged goes in; what comes
    /// out is the subset the finished event still holds two declarations for,
    /// which is exactly what a single event-sized transaction would have
    /// reported. One read transaction, because an event asks about every
    /// identity it flagged at once.
    pub fn definition_collisions(&self, ids: &BTreeSet<NodeId>) -> Result<Vec<NodeId>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for id in ids {
            if let Some(record) = read_node(&nodes, id)?
                && record.is_multi_file_definition()
            {
                out.push(*id);
            }
        }
        Ok(out)
    }

    /// Current declaration definitions for the requested multi-file nodes.
    ///
    /// Sites are already sorted by `(file, line)`, so the returned vectors
    /// have one deterministic order regardless of batch or creation order.
    pub fn collision_definitions(
        &self,
        ids: &BTreeSet<NodeId>,
    ) -> Result<BTreeMap<NodeId, Vec<Definition>>, String> {
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
        let mut out = BTreeMap::new();
        for id in ids {
            let Some(record) = read_node(&nodes, id)? else {
                continue;
            };
            if !record.is_multi_file_definition() {
                continue;
            }
            let mut definitions = Vec::with_capacity(record.declarations().len());
            for site in record.declarations() {
                let stored = site.merge_definition.as_ref().ok_or_else(|| {
                    format!(
                        "definition {}:{} has no stored merge facts",
                        site.file, site.line
                    )
                })?;
                definitions.push(stored.definition()?);
            }
            out.insert(*id, definitions);
        }
        Ok(out)
    }

    /// Persist language collision verdicts after phase 1 and before any file
    /// regains its currency claim in phase 2.
    pub fn set_collision_dispositions(
        &self,
        touched: &BTreeSet<NodeId>,
        dispositions: &BTreeMap<NodeId, CollisionDisposition>,
    ) -> Result<(), String> {
        if touched.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut table = txn
                .open_table(COLLISION_DISPOSITIONS)
                .map_err(|e| e.to_string())?;
            let mut invalidated_verdicts = txn
                .open_table(COLLISION_VERDICTS_INVALIDATED)
                .map_err(|e| e.to_string())?;
            for id in touched {
                match dispositions.get(id) {
                    Some(disposition) => {
                        table
                            .insert(id, disposition.code())
                            .map_err(|e| e.to_string())?;
                    }
                    None => {
                        table.remove(id).map_err(|e| e.to_string())?;
                    }
                }
                invalidated_verdicts.remove(id).map_err(|e| e.to_string())?;
            }
        }
        txn.commit().map_err(|e| e.to_string())
    }

    /// Declaring files for these identities, excluding files already leaving.
    ///
    /// A deletion can change a persisted collision verdict without applying
    /// a definition half for any surviving file. The pipeline withdraws these
    /// files' currency claims before deleting, then restores them through its
    /// normal waking round after the new verdict is durable.
    pub fn declaration_files(
        &self,
        ids: &BTreeSet<NodeId>,
        excluding: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>, String> {
        if ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
        let mut out = BTreeSet::new();
        for id in ids {
            let Some(record) = read_node(&nodes, id)? else {
                continue;
            };
            if !record.is_multi_file_definition() {
                continue;
            }
            for site in record.declarations() {
                if !excluding.contains(&site.file) {
                    out.insert(site.file.clone());
                }
            }
        }
        Ok(out)
    }

    /// Replace the supertype half of every file in the batch, in one
    /// transaction.
    ///
    /// A file with no types to state is *removed* rather than written empty:
    /// most files declare no type at all, and a row per file would be a table
    /// the size of the tree carrying nothing. Removal is also what keeps a
    /// warm store byte-identical to a cold one, which the snapshot oracle
    /// compares.
    ///
    /// Carries the same currency rule [`Store::apply_defs`] does, for the
    /// same reason: what a type sits under decides every member lookup
    /// beneath it, so a row that moves invalidates the files that read it,
    /// and the invalidation is committed with the row rather than after it.
    /// The files it withdrew are named back to the caller.
    pub fn apply_supers(&self, batch: &SuperBatch) -> Result<BTreeSet<String>, String> {
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        let invalidated;
        {
            let mut supers = txn.open_table(SUPERS).map_err(|e| e.to_string())?;
            let mut files = txn.open_table(FILES).map_err(|e| e.to_string())?;
            let cands = txn
                .open_multimap_table(CANDIDATES)
                .map_err(|e| e.to_string())?;
            // The identities whose relation this transaction moves. A file's
            // row is its own contribution to a merged relation, so a
            // contribution that changed is an identity whose closure may have
            // — the over-approximation costs a re-resolve and never an answer.
            let mut moved: BTreeSet<NodeId> = BTreeSet::new();
            for file in &batch.files {
                let previous: BTreeMap<NodeId, SuperRecord> =
                    match supers.get(file.path.as_str()).map_err(|e| e.to_string())? {
                        Some(guard) => decode::<Vec<(NodeId, SuperRecord)>>(guard.value())?
                            .into_iter()
                            .collect(),
                        None => BTreeMap::new(),
                    };
                let mut rows = file.types.clone();
                rows.sort_by_key(|row| row.0);
                let stated: BTreeMap<NodeId, &SuperRecord> =
                    rows.iter().map(|(id, record)| (*id, record)).collect();
                for id in previous.keys().chain(stated.keys()) {
                    if previous.get(id) != stated.get(id).copied() {
                        moved.insert(*id);
                    }
                }
                // This is a third of a file's facts, so the claim goes with
                // the same rule phase 1 applies: only `apply_refs` restores
                // it. Every file here has already been through `apply_defs`
                // in this event, so this is a re-assertion — and it is the
                // invariant that is worth stating locally, not the saving.
                withdraw_claim(&mut files, &file.path)?;
                if rows.is_empty() {
                    supers
                        .remove(file.path.as_str())
                        .map_err(|e| e.to_string())?;
                    continue;
                }
                let bytes = encode(&rows)?;
                supers
                    .insert(file.path.as_str(), bytes.as_slice())
                    .map_err(|e| e.to_string())?;
            }
            let rewritten: HashSet<&str> =
                batch.files.iter().map(|file| file.path.as_str()).collect();
            invalidated = invalidate_probers(&cands, &mut files, &moved, &rewritten)?;
        }
        txn.commit().map_err(|e| e.to_string())?;
        Ok(invalidated)
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
                    .insert(file.path.as_str(), file.hash.as_slice())
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
    ///
    /// A deletion is the sharpest way an identity can move, so this carries
    /// the same currency rule [`Store::apply_defs`] does: the files whose
    /// stored resolution consulted an identity these files were the last to
    /// declare lose their claim in this transaction, and are named back to
    /// the caller to be re-resolved.
    pub fn forget_files(&self, paths: &[String]) -> Result<BTreeSet<String>, String> {
        if paths.is_empty() {
            return Ok(BTreeSet::new());
        }
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        let invalidated;
        {
            let mut nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
            let mut dispositions = txn
                .open_table(COLLISION_DISPOSITIONS)
                .map_err(|e| e.to_string())?;
            let mut invalidated_verdicts = txn
                .open_table(COLLISION_VERDICTS_INVALIDATED)
                .map_err(|e| e.to_string())?;
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
            // What the identities these files declare meant before they went,
            // and the identities whose supertype relation loses a
            // contribution. External nodes are deliberately absent: no
            // resolver can probe one — the `external:` prefix is unreachable
            // from any candidate — so one appearing or going invalidates
            // nothing.
            let mut before: BTreeMap<NodeId, Option<NodePayload>> = BTreeMap::new();
            let mut moved: BTreeSet<NodeId> = BTreeSet::new();
            let mut dropped_definitions: BTreeSet<NodeId> = BTreeSet::new();
            for path in paths {
                forget_ref_half(
                    &mut nodes, &mut refs, &mut edges, &mut rev, &mut cands, &ref_owned, path,
                )?;
                ref_owned.remove(path.as_str()).map_err(|e| e.to_string())?;
                if let Some(previous) = read_owned::<DefOwned>(&def_owned, path)? {
                    for id in &previous.nodes {
                        note_payload(&nodes, &mut before, id)?;
                        drop_site(&mut nodes, id, path)?;
                        dropped_definitions.insert(*id);
                    }
                }
                def_owned.remove(path.as_str()).map_err(|e| e.to_string())?;
                if let Some(guard) = supers.get(path.as_str()).map_err(|e| e.to_string())? {
                    for (id, _) in decode::<Vec<(NodeId, SuperRecord)>>(guard.value())? {
                        moved.insert(id);
                    }
                }
                supers.remove(path.as_str()).map_err(|e| e.to_string())?;
                files.remove(path.as_str()).map_err(|e| e.to_string())?;
            }
            for (id, was) in &before {
                if read_node(&nodes, id)?
                    .map(|record| record.payload())
                    .as_ref()
                    != was.as_ref()
                {
                    moved.insert(*id);
                }
            }
            let gone: HashSet<&str> = paths.iter().map(String::as_str).collect();
            let mut changed = invalidate_probers(&cands, &mut files, &moved, &gone)?;
            for id in dropped_definitions {
                // The persisted value answered for the set before this file
                // left. It cannot remain publishable for its survivors.
                let had_verdict = dispositions
                    .remove(&id)
                    .map_err(|e| e.to_string())?
                    .is_some();
                if had_verdict
                    || invalidated_verdicts
                        .get(&id)
                        .map_err(|e| e.to_string())?
                        .is_some()
                {
                    invalidated_verdicts
                        .insert(&id, 1)
                        .map_err(|e| e.to_string())?;
                }
                let Some(record) = read_node(&nodes, &id)? else {
                    continue;
                };
                if !record.is_multi_file_definition() {
                    continue;
                }
                for site in record.declarations() {
                    if gone.contains(site.file.as_str()) {
                        continue;
                    }
                    withdraw_known(&mut files, &site.file)?;
                    changed.insert(site.file.clone());
                }
            }
            invalidated = changed;
        }
        txn.commit().map_err(|e| e.to_string())?;
        Ok(invalidated)
    }

    /// Take back the store's claim that these files' facts are current,
    /// keeping the facts themselves.
    ///
    /// What a scan does with an owned file it could not read. Stepping over
    /// one and keeping its halves is right — a permission bit is not a
    /// deletion — but the halves were resolved against a graph this event has
    /// since moved, and the stored hash still matches the file's untouched
    /// bytes. Left alone, the file is never in a later scan's changed set and
    /// never woken by one either, because waking is driven by the identities
    /// *this* event moved: its rows keep outcomes computed against a graph
    /// state that no longer exists, and no report ever says so again.
    ///
    /// Clearing the hash puts the file in the next successful scan's changed
    /// set, which re-reads and re-resolves it in full. The row survives, so
    /// the file is still one [`Store::known_files`] names and a walk that
    /// stops reaching it is still read as a deletion.
    pub fn forget_hashes(&self, paths: &[String]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut files = txn.open_table(FILES).map_err(|e| e.to_string())?;
            for path in paths {
                // Only a file the store already knows. A file that failed on
                // its very first scan has no facts to go stale, and minting a
                // row for it here would make the next walk that does not
                // reach it look like a deletion of nothing.
                if files
                    .get(path.as_str())
                    .map_err(|e| e.to_string())?
                    .is_some()
                {
                    files
                        .insert(path.as_str(), [].as_slice())
                        .map_err(|e| e.to_string())?;
                }
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

    /// The bytes this file's stored facts were computed from, when the store
    /// still claims they are current.
    ///
    /// `None` covers two situations a scan treats identically: the store has
    /// never seen the file, and the store has taken its claim back because a
    /// scan could not read it. Both mean "re-read this file", which is what
    /// the caller does with an answer that does not match.
    pub fn file_hash(&self, path: &str) -> Result<Option<[u8; 32]>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(FILES).map_err(|e| e.to_string())?;
        Ok(table
            .get(path)
            .map_err(|e| e.to_string())?
            .and_then(|guard| <[u8; 32]>::try_from(guard.value()).ok()))
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
    /// An edge is a reference that *linked*: `Resolved` to a definition in
    /// this repository, or `External` to the dependency node naming the
    /// package it reached. An `Unresolved` reference produces none, and
    /// nothing else produces one. `src` is the node the reference sat in
    /// either way — which is the assertion worth making about it.
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
    /// Facets come straight off the record. Still nothing *shared* branches
    /// on them — that is what keeps them a bitset rather than a [`DefKind`]
    /// variant each — but the owning resolver may, and could not before they
    /// were stored.
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
                NodeRecord::Definition { kind, facets, .. } => Entry::Definition {
                    kind: DefKind::from_code(kind)
                        .ok_or_else(|| format!("stored node kind {kind} has no variant"))?,
                    facets: DefFacets::from_bits(facets),
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
            collision_dispositions: BTreeMap::new(),
            rows: BTreeMap::new(),
            edges: BTreeSet::new(),
            candidates: BTreeMap::new(),
            supers: BTreeMap::new(),
        };
        let files = txn.open_table(FILES).map_err(|e| e.to_string())?;
        for entry in files.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            snapshot.files.insert(
                key.value().to_string(),
                <[u8; 32]>::try_from(value.value()).ok(),
            );
        }
        let nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
        for entry in nodes.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            snapshot.nodes.insert(*key.value(), decode(value.value())?);
        }
        let dispositions = txn
            .open_table(COLLISION_DISPOSITIONS)
            .map_err(|e| e.to_string())?;
        for entry in dispositions.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            snapshot.collision_dispositions.insert(
                *key.value(),
                CollisionDisposition::from_code(value.value())?,
            );
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
        let dispositions = txn
            .open_table(COLLISION_DISPOSITIONS)
            .map_err(|e| e.to_string())?;
        let invalidated_verdicts = txn
            .open_table(COLLISION_VERDICTS_INVALIDATED)
            .map_err(|e| e.to_string())?;
        for entry in nodes.iter().map_err(|e| e.to_string())? {
            let (id, value) = entry.map_err(|e| e.to_string())?;
            let record: NodeRecord = decode(value.value())?;
            if !record.is_multi_file_definition() {
                continue;
            }
            if invalidated_verdicts
                .get(id.value())
                .map_err(|e| e.to_string())?
                .is_some()
            {
                continue;
            }
            let disposition = dispositions
                .get(id.value())
                .map_err(|e| e.to_string())?
                .map(|guard| CollisionDisposition::from_code(guard.value()))
                .transpose()?
                // Direct Store callers have no language resolver. A fresh
                // multi-file definition remains the mechanical Collision
                // answer until a resolver supplies Mergeable.
                .unwrap_or(CollisionDisposition::Collision);
            if disposition == CollisionDisposition::Collision {
                report.fqn_collisions += 1;
            }
        }
        Ok(report)
    }
}

/// Where a store that does not exist yet is built before it is published.
///
/// A sibling of the store's own path, so the publication below is a link
/// within one directory and therefore within one filesystem — a store staged
/// under `/tmp` and published onto another mount could not be linked at all.
/// The name is derived rather than unique on purpose: it is the *lock*. Two
/// scans that both find no store both open this one path, and redb's
/// `flock(2)` refuses the second exactly as it refuses a second writer on a
/// store that already exists. A unique name per process would let both build
/// one and both publish.
fn staging_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(".new");
    PathBuf::from(name)
}

/// Turn redb's word for a failed open into one that names the store and the
/// way out.
///
/// Shared by both open paths for the two failures that read the same from
/// either side. redb says `Database already open. Cannot acquire lock.` and
/// `I/O error: invalid data`, and neither names a file, a holder, or a
/// remedy — see [`HELD_FOR_WRITING`] and [`NOT_A_STORE`].
fn open_failure(path: &Path, e: redb::DatabaseError) -> String {
    match e {
        redb::DatabaseError::DatabaseAlreadyOpen => {
            format!("{}: {HELD_FOR_WRITING}", path.display())
        }
        e if is_not_a_database(&e) => format!("{}: {NOT_A_STORE}", path.display()),
        other => format!("{}: {other}", path.display()),
    }
}

/// Whether redb refused these bytes because they are not a database.
///
/// The magic number is absent: the file was never finished being created, or
/// it was never a store. redb reports both as an `InvalidData` I/O error,
/// which is the one redb failure that no retry and no repair gets past.
fn is_not_a_database(e: &redb::DatabaseError) -> bool {
    matches!(
        e,
        redb::DatabaseError::Storage(redb::StorageError::Io(io))
            if io.kind() == std::io::ErrorKind::InvalidData
    )
}

/// Build an empty database beside `path` and publish it there whole.
///
/// Called only when nothing at `path` holds bytes. What it leaves is a *bare*
/// redb
/// database — no schema stamp and no tables — because that is the state
/// [`Store::open`] already knows how to finish, and giving the same work two
/// implementations is how they drift apart. A process killed after the
/// publication and before the stamp lands opens a store whose generation is
/// absent, which is the wipe-and-rebuild path, unchanged.
fn create_beside(path: &Path) -> Result<(), String> {
    let staging = staging_path(path);
    {
        let build = || {
            redb::Builder::new()
                .set_cache_size(CACHE_BYTES)
                .create(&staging)
        };
        let _db = match build() {
            Ok(db) => db,
            // Another scan is building the same store right now. Its path is
            // the one worth naming: the staging file is an implementation
            // detail of the store the caller asked for.
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                return Err(format!("{}: {HELD_FOR_WRITING}", path.display()));
            }
            // A staging file an earlier scan was killed while making. Nobody
            // holds it — the lock above would have said so — and the name is
            // arthron's own, derived from the store the caller named, so this
            // is the one file it may take back. Truncated in place rather
            // than unlinked, so that a scan racing this one locks the same
            // inode and is refused instead of building a second store.
            Err(e) if is_not_a_database(&e) => {
                fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&staging)
                    .map_err(|e| format!("{}: {e}", staging.display()))?;
                build().map_err(|e| open_failure(&staging, e))?
            }
            Err(other) => return Err(open_failure(&staging, other)),
        };
        // Dropped here: the file is closed and its lock released before the
        // link, so the store the caller ends up with is the one `Store::open`
        // opens by name below, holding the only handle anybody has to it.
    }
    publish(&staging, path)
}

/// Move a finished staging database onto the store's own path.
///
/// [`fs::hard_link`] and not [`fs::rename`], because rename replaces. Two
/// scans that both found no store race here, and a rename by the loser would
/// unlink the winner's store while the winner is writing into it — leaving a
/// scan that reports a rate for a graph nothing can ever read. A link refuses
/// an occupied path, so the loser simply drops what it built and opens what
/// it found.
///
/// The rename is the fallback for a filesystem with no links at all (FAT),
/// where the choice is between a small race and no store whatsoever.
fn publish(staging: &Path, path: &Path) -> Result<(), String> {
    let mut linked = fs::hard_link(staging, path).is_ok();
    // A path holding no bytes is not a store and is not data — a `touch` is
    // the usual way one gets there. redb would initialise it where it lies,
    // which is the window all of this exists to close, so it is taken out of
    // the way. The link is *retried* rather than assumed: a scan that
    // published a real store into the gap still wins it.
    if !linked && !holds_bytes(path) {
        let _ = fs::remove_file(path);
        linked = fs::hard_link(staging, path).is_ok();
    }
    if linked || path.exists() {
        let _ = fs::remove_file(staging);
    } else {
        fs::rename(staging, path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    sync_dir(path);
    Ok(())
}

/// Whether a path holds anything a store could be read out of.
///
/// A missing file and an empty one are the same answer, because they are the
/// same answer to redb: both are a database it is willing to make from
/// nothing. Everything else — a real store, a wedged one, a tarball someone
/// aimed `--db` at — holds bytes, and bytes at a path the caller named are
/// never taken away by a scan.
fn holds_bytes(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.len() > 0)
}

/// Make the new directory entry durable, best effort.
///
/// The link is what publishes the store, and a link is a directory write. The
/// file's own bytes are already synced — redb did that before it wrote the
/// magic number — so the worst a lost entry costs is a store that is not
/// there, which the next scan reads as "nothing here yet" and rebuilds. That
/// is why this is best effort: platforms that will not open a directory as a
/// file lose nothing that is not already recoverable.
fn sync_dir(path: &Path) {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
}

/// A read-only handle on a graph some earlier scan wrote.
///
/// A separate type rather than a mode of [`Store`], because the two differ in
/// everything a reader must not do. [`Store::open`] *creates* the file, takes
/// redb's write lock, and wipes the tables outright when the store carries
/// another [`SCHEMA_VERSION`]. A query that did any of those would answer a
/// question by destroying the answer — and a query run by habit against a
/// path with a typo in it would silently mint an empty graph and report that
/// the name is not in the repository.
///
/// It also gives the concurrency answer for free. redb takes an exclusive
/// file lock for writing, so opening read-only while a scan holds the store
/// fails immediately with [`redb::DatabaseError::DatabaseAlreadyOpen`] rather
/// than blocking or reading a half-written transaction — see
/// [`ReadStore::open`], which is where that becomes a sentence a person can
/// act on.
pub struct ReadStore {
    db: ReadOnlyDatabase,
}

// Hand-written because a redb database is not `Debug` and because there is
// nothing about an open handle worth printing. It exists so that
// `ReadStore::open(..).expect_err(..)` compiles for callers testing the
// refusals this type is largely built to give.
impl std::fmt::Debug for ReadStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadStore")
    }
}

impl ReadStore {
    /// Open an existing store for reading only.
    ///
    /// Five failures, each named rather than collapsed into "cannot open":
    /// the file is not there, a scan is holding it for writing, an
    /// interrupted scan left it needing recovery, it is not a store at all,
    /// or it was written under a different schema generation. The last two
    /// are refusals and not repairs: a reader that rebuilt the store to
    /// satisfy itself would destroy a graph whose owner never asked for one —
    /// see [`NEEDS_RECOVERY`] and [`NOT_A_STORE`].
    pub fn open(path: &Path) -> Result<Self, String> {
        let db = redb::Builder::new()
            .set_cache_size(CACHE_BYTES)
            .open_read_only(path)
            .map_err(|e| match e {
                // Recovery is a write. redb aborts it under a read-only open
                // rather than doing it, which is the right answer and an
                // unreadable way of giving it — see [`NEEDS_RECOVERY`].
                redb::DatabaseError::RepairAborted => {
                    format!("{}: {NEEDS_RECOVERY}", path.display())
                }
                other => open_failure(path, other),
            })?;
        let store = ReadStore { db };
        match store.schema_version()? {
            Some(SCHEMA_VERSION) => Ok(store),
            // No `meta` table and no key both mean the same thing: nothing
            // ever stamped a generation here, so nothing may be read out of
            // it as though a resolver had.
            found => Err(format!(
                "{}: schema generation {} — this build reads {SCHEMA_VERSION}; re-run `arthron scan`",
                path.display(),
                found.map_or_else(|| "absent".to_string(), |v| v.to_string()),
            )),
        }
    }

    /// The files the store holds facts for and no longer claims are current.
    ///
    /// The one predicate that says a store is not wholly answerable: a
    /// [`FILES`] row whose value is not a hash. Both things that leave one —
    /// a scan that could not read an owned file, and a scan killed between a
    /// file's two halves — mean the same to a reader, and the reader is the
    /// only surface that can say so, because the scan that would have said it
    /// is the one that did not finish. See [`NOT_ALL_CURRENT`].
    ///
    /// A count and not the paths: every caller of this asks one question —
    /// may I answer without a caveat — and materialising a path per file
    /// would make the caveat cost a whole-table string allocation on a store
    /// where the answer is almost always zero.
    pub fn not_current(&self) -> Result<usize, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        // A store no scan ever wrote to has no `files` table, which is no
        // files rather than a failure — the same reading `schema_version`
        // gives an absent `meta`.
        let Ok(table) = txn.open_table(FILES) else {
            return Ok(0);
        };
        let mut count = 0;
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (_, value) = entry.map_err(|e| e.to_string())?;
            if <[u8; 32]>::try_from(value.value()).is_err() {
                count += 1;
            }
        }
        Ok(count)
    }

    /// The generation stamped in [`META`], if the store carries one.
    fn schema_version(&self) -> Result<Option<u32>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        // A store no write transaction ever touched has no `meta` table at
        // all, which is an absent generation and not an I/O failure.
        let Ok(meta) = txn.open_table(META) else {
            return Ok(None);
        };
        Ok(meta
            .get(SCHEMA_VERSION_KEY)
            .map_err(|e| e.to_string())?
            .and_then(|guard| <[u8; 4]>::try_from(guard.value()).ok())
            .map(u32::from_le_bytes))
    }

    /// The record stored under a node id, if the node exists.
    pub fn node(&self, id: &NodeId) -> Result<Option<NodeRecord>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(NODES).map_err(|e| e.to_string())?;
        read_node(&table, id)
    }

    /// Visit every node in the graph, in identity order.
    ///
    /// A visitor and not a `Vec`, because the caller decides what to keep: a
    /// name index wants a string and a kind per node and nothing else, and
    /// materialising every declaration site of every node in a large
    /// repository to build one would cost orders of magnitude more than the
    /// index it produced.
    pub fn for_each_node(
        &self,
        mut visit: impl FnMut(NodeId, NodeRecord) -> Result<(), String>,
    ) -> Result<(), String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(NODES).map_err(|e| e.to_string())?;
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            visit(*key.value(), decode(value.value())?)?;
        }
        Ok(())
    }

    /// Visit every reference row in the graph, in key order.
    ///
    /// Whole-table and not an index lookup on purpose. The candidate index
    /// would answer "which rows probed this identity" far faster, but it is
    /// the *invalidation* index: it holds exactly what each resolver declared
    /// it read, and a resolver that under-declared would make this query drop
    /// reference sites without saying so. The rows are the store's own record
    /// of what resolved, and reading them is the only answer that cannot be
    /// wrong in that direction.
    pub fn for_each_row(
        &self,
        mut visit: impl FnMut(RefKey, RefRecord) -> Result<(), String>,
    ) -> Result<(), String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(REFS).map_err(|e| e.to_string())?;
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (key, value) = entry.map_err(|e| e.to_string())?;
            let (file, encoded) = key.value();
            visit(RefKey::join(file, encoded)?, decode(value.value())?)?;
        }
        Ok(())
    }

    /// Every node with an edge *into* this one, with the edge's
    /// [`crate::model::RefKind`] code.
    ///
    /// Served by [`REV_EDGES`], which is keyed `(dst, src, kind)`, so one
    /// node's predecessors are one contiguous range rather than a scan of
    /// every edge in the repository. That is what makes a layered reverse
    /// closure affordable at all.
    pub fn edges_into(&self, dst: &NodeId) -> Result<Vec<(NodeId, u8)>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(REV_EDGES).map_err(|e| e.to_string())?;
        let (low, high) = ([0u8; 16], [0xffu8; 16]);
        let mut out = Vec::new();
        for entry in table
            .range((dst, &low, u8::MIN)..=(dst, &high, u8::MAX))
            .map_err(|e| e.to_string())?
        {
            let (key, _) = entry.map_err(|e| e.to_string())?;
            let (_, src, kind) = key.value();
            out.push((*src, kind));
        }
        Ok(out)
    }
}

/// Drop every graph table. [`META`] is deliberately untouched: it carries
/// the generation and fingerprint that decided the drop.
fn drop_graph(txn: &redb::WriteTransaction) -> Result<(), String> {
    txn.delete_table(NODES).map_err(|e| e.to_string())?;
    txn.delete_table(COLLISION_DISPOSITIONS)
        .map_err(|e| e.to_string())?;
    txn.delete_table(COLLISION_VERDICTS_INVALIDATED)
        .map_err(|e| e.to_string())?;
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
    txn.open_table(COLLISION_DISPOSITIONS)
        .map_err(|e| e.to_string())?;
    txn.open_table(COLLISION_VERDICTS_INVALIDATED)
        .map_err(|e| e.to_string())?;
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

/// What an identity meant before this transaction touched it, recorded once.
///
/// Called immediately before the first write to a node, and never again for
/// that node in the same transaction: the point of comparison is the graph as
/// the transaction found it, not as it left it halfway through.
fn note_payload(
    table: &NodeTable<'_>,
    before: &mut BTreeMap<NodeId, Option<NodePayload>>,
    id: &NodeId,
) -> Result<(), String> {
    if before.contains_key(id) {
        return Ok(());
    }
    let payload = read_node(table, id)?.map(|record| record.payload());
    before.insert(*id, payload);
    Ok(())
}

/// What an identity's complete declaration set was before this transaction.
///
/// Payload equality is enough for resolver invalidation, but not for a
/// collision disposition: adding, removing or replacing a declaration site
/// can leave the representative payload unchanged while changing the pairs a
/// language must classify.
fn note_node(
    table: &NodeTable<'_>,
    before: &mut BTreeMap<NodeId, Option<NodeRecord>>,
    id: &NodeId,
) -> Result<(), String> {
    if before.contains_key(id) {
        return Ok(());
    }
    before.insert(*id, read_node(table, id)?);
    Ok(())
}

/// Withdraw the store's claim that a file's stored facts are current, and
/// record the file as one the store holds facts for if it does not already.
///
/// What a transaction writing *part* of a file's facts owes: the claim is a
/// statement about the whole file, and until every half is committed there is
/// no whole to make it about. Minting the row is half the point — a file
/// whose definitions are stored under no [`FILES`] row is a file no walk can
/// ever read as deleted, so its nodes would outlive it with nothing left to
/// name them.
fn withdraw_claim(files: &mut BytesTable<'_>, path: &str) -> Result<(), String> {
    let withdrawn = files
        .get(path)
        .map_err(|e| e.to_string())?
        .is_some_and(|guard| guard.value().is_empty());
    if !withdrawn {
        files
            .insert(path, [].as_slice())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Withdraw the claim for a file the store already knows, and leave one it
/// does not alone.
///
/// The difference from [`withdraw_claim`] is deliberate: this is called for
/// *other people's* files, and minting a row for one the store holds nothing
/// about would make the next walk that does not reach it look like a deletion
/// of nothing — the same trap [`Store::forget_hashes`] avoids.
fn withdraw_known(files: &mut BytesTable<'_>, path: &str) -> Result<(), String> {
    let claimed = files
        .get(path)
        .map_err(|e| e.to_string())?
        .is_some_and(|guard| !guard.value().is_empty());
    if claimed {
        files
            .insert(path, [].as_slice())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Withdraw the currency claim of every file whose stored resolution
/// consulted an identity this transaction moved, and name them.
///
/// The candidate index is the record of what each resolver read — hits and
/// misses alike — so it is exactly the list of rows an identity's meaning
/// moving invalidates. Writing the withdrawal *here*, inside the transaction
/// that moved the identity, is what makes a scan killed a moment later
/// recoverable: the store no longer claims to be current for a file whose
/// answer it has just falsified, so the next scan re-reads it.
///
/// `rewritten` is the transaction's own files, which are being replaced in
/// full and have had their claims withdrawn already.
///
/// The empty-index short circuit is what keeps a cold scan free: phase 1 runs
/// to completion before the first candidate is written, so on a store nobody
/// has resolved into yet there is nothing here to invalidate and no lookup
/// worth making.
fn invalidate_probers(
    cands: &CandidateTable<'_>,
    files: &mut BytesTable<'_>,
    moved: &BTreeSet<NodeId>,
    rewritten: &HashSet<&str>,
) -> Result<BTreeSet<String>, String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    if moved.is_empty() || cands.is_empty().map_err(|e| e.to_string())? {
        return Ok(out);
    }
    for id in moved {
        for value in cands.get(id).map_err(|e| e.to_string())? {
            let guard = value.map_err(|e| e.to_string())?;
            let (file, _) = guard.value();
            if rewritten.contains(file) || out.contains(file) {
                continue;
            }
            out.insert(file.to_string());
        }
    }
    for path in &out {
        withdraw_known(files, path)?;
    }
    Ok(out)
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
        NodeRecord::Definition {
            kind,
            facets,
            targets,
            ..
        } => {
            // Kind and facets come out of one site together: they are two
            // halves of what a single file said this declaration is, and
            // taking them from different sites would state a declaration no
            // file wrote.
            if let Some((k, f)) = sites.iter().find_map(|s| match s.payload {
                NodePayload::Definition(k, f) => Some((k, f)),
                NodePayload::Alias(k, f, _) => Some((k, f)),
                _ => None,
            }) {
                *kind = k;
                *facets = f;
            }
            // Every surviving site's targets, in site order. A union and not
            // a first-wins pick: two files may legitimately declare one alias
            // key — that is what a star export re-exported from two places
            // is — and dropping either would make the walk miss a name the
            // corpus really does export.
            let mut merged: Vec<NodeId> = Vec::new();
            for site in &sites {
                if let NodePayload::Alias(_, _, ts) = &site.payload {
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
            payload: NodePayload::Definition(0, 0),
            merge_definition: None,
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
                        facets: 0,
                        targets: Vec::new(),
                        declarations: vec![site("pkg/a.go", 3)],
                    },
                )],
            }],
        };
        assert!(
            store
                .apply_defs(&defs)
                .expect("apply defs")
                .colliding
                .is_empty()
        );

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
        // Both files, and only one of them current. `pkg/a.go` is here on
        // the strength of its phase-1 half alone, with no hash: the store
        // holds facts for it, so a walk that stops reaching it has to read
        // that as a deletion — a definition stored under no file row is a
        // node nothing can ever take away. Its claim is withdrawn until
        // `apply_refs` lands the other half.
        assert_eq!(
            store.known_files().unwrap(),
            vec!["pkg/a.go".to_string(), "pkg/b.go".to_string()]
        );
        assert_eq!(store.file_hash("pkg/a.go").unwrap(), None);
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
                        facets: 0,
                        targets: Vec::new(),
                        declarations: vec![site(path, line)],
                    },
                ),
            ],
        };
        let batch = DefBatch {
            files: vec![declare("pkg/a_linux.go", 3), declare("pkg/a_darwin.go", 4)],
        };
        let colliding = store.apply_defs(&batch).expect("apply defs").colliding;
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
