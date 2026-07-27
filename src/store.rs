//! The durable graph: redb tables, bincode records, batched writes.
//!
//! This layer interprets nothing — it stores what the resolver decided and
//! tallies it back out. One write transaction per [`Batch`] (batch per
//! event: 500 files in one transaction measured 60ms against 216ms as 500
//! separate transactions).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use bincode::{Decode, Encode, config};
use redb::{Database, MultimapTableDefinition, ReadableDatabase, ReadableTable, TableDefinition};

use crate::model::NodeId;

const NODES: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("nodes");
const REFS: TableDefinition<(&str, u8, &str), &[u8]> = TableDefinition::new("refs");
const EDGES: TableDefinition<(&[u8; 16], &[u8; 16], u8), ()> = TableDefinition::new("edges");
const REV_EDGES: TableDefinition<(&[u8; 16], &[u8; 16], u8), ()> =
    TableDefinition::new("rev_edges");
const CANDIDATES: MultimapTableDefinition<&[u8; 16], (&str, u8, &str)> =
    MultimapTableDefinition::new("candidates");
const FILES: TableDefinition<&str, &[u8; 32]> = TableDefinition::new("files");

/// A stored node: something a reference can name.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum NodeRecord {
    /// A definition inside this repository.
    Definition {
        /// Canonical fully-qualified name.
        fqn: String,
        /// [`crate::model::DefKind`] code.
        kind: u8,
        /// Repo-relative file path (a field, not a node).
        file: String,
        /// 1-based line of the declaration.
        line: u32,
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
    },
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

/// One deduplicated reference row: outcome, occurrence count, first site.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct RefRecord {
    /// The single outcome for this reference.
    pub outcome: StoredOutcome,
    /// How many times this (file, kind, raw_target) occurs.
    pub count: u32,
    /// 1-based line of the first occurrence.
    pub first_line: u32,
    /// [`crate::model::Lang`] code.
    pub lang: u8,
}

/// Everything one indexing event writes, applied in one transaction.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    /// (path, content hash) for every file this event covers.
    pub files: Vec<(String, [u8; 32])>,
    /// Nodes to upsert.
    pub nodes: Vec<(NodeId, NodeRecord)>,
    /// Reference rows: (file, kind code, raw_target, record).
    pub refs: Vec<(String, u8, String, RefRecord)>,
    /// Resolved edges (src, dst, kind code).
    pub edges: Vec<(NodeId, NodeId, u8)>,
    /// Candidate-index entries: candidate id → referencing row key.
    pub candidates: Vec<(NodeId, (String, u8, String))>,
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
}

/// Handle on the on-disk graph.
pub struct Store {
    db: Database,
}

impl Store {
    /// Open (or create) the database and its tables.
    pub fn open(path: &Path) -> Result<Self, String> {
        let db = Database::create(path).map_err(|e| e.to_string())?;
        let txn = db.begin_write().map_err(|e| e.to_string())?;
        {
            txn.open_table(NODES).map_err(|e| e.to_string())?;
            txn.open_table(REFS).map_err(|e| e.to_string())?;
            txn.open_table(EDGES).map_err(|e| e.to_string())?;
            txn.open_table(REV_EDGES).map_err(|e| e.to_string())?;
            txn.open_multimap_table(CANDIDATES)
                .map_err(|e| e.to_string())?;
            txn.open_table(FILES).map_err(|e| e.to_string())?;
        }
        txn.commit().map_err(|e| e.to_string())?;
        Ok(Store { db })
    }

    /// Apply one batch in one write transaction.
    pub fn apply(&self, batch: &Batch) -> Result<(), String> {
        let txn = self.db.begin_write().map_err(|e| e.to_string())?;
        {
            let mut files = txn.open_table(FILES).map_err(|e| e.to_string())?;
            for (path, hash) in &batch.files {
                files
                    .insert(path.as_str(), hash)
                    .map_err(|e| e.to_string())?;
            }
            let mut nodes = txn.open_table(NODES).map_err(|e| e.to_string())?;
            for (id, record) in &batch.nodes {
                let bytes = bincode::encode_to_vec(record, config::standard())
                    .map_err(|e| e.to_string())?;
                nodes
                    .insert(id, bytes.as_slice())
                    .map_err(|e| e.to_string())?;
            }
            let mut refs = txn.open_table(REFS).map_err(|e| e.to_string())?;
            for (file, kind, raw, record) in &batch.refs {
                let bytes = bincode::encode_to_vec(record, config::standard())
                    .map_err(|e| e.to_string())?;
                refs.insert((file.as_str(), *kind, raw.as_str()), bytes.as_slice())
                    .map_err(|e| e.to_string())?;
            }
            let mut edges = txn.open_table(EDGES).map_err(|e| e.to_string())?;
            let mut rev = txn.open_table(REV_EDGES).map_err(|e| e.to_string())?;
            for (src, dst, kind) in &batch.edges {
                edges
                    .insert((src, dst, *kind), ())
                    .map_err(|e| e.to_string())?;
                rev.insert((dst, src, *kind), ())
                    .map_err(|e| e.to_string())?;
            }
            let mut cands = txn
                .open_multimap_table(CANDIDATES)
                .map_err(|e| e.to_string())?;
            for (cand, (file, kind, raw)) in &batch.candidates {
                cands
                    .insert(cand, (file.as_str(), *kind, raw.as_str()))
                    .map_err(|e| e.to_string())?;
            }
        }
        txn.commit().map_err(|e| e.to_string())
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
        let Some(guard) = table.get(id).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let (record, _): (NodeRecord, usize) =
            bincode::decode_from_slice(guard.value(), config::standard())
                .map_err(|e| e.to_string())?;
        Ok(Some(record))
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
            let (record, _): (NodeRecord, usize) =
                bincode::decode_from_slice(value.value(), config::standard())
                    .map_err(|e| e.to_string())?;
            if let NodeRecord::Package {
                import_path,
                name: Some(name),
            } = record
            {
                out.insert(import_path, name);
            }
        }
        Ok(out)
    }

    /// All node ids, as the pipeline's symbol-probe set.
    ///
    /// Skeleton shortcut: loaded per phase-2 run, never maintained
    /// incrementally — the store remains the symbol table of record.
    pub fn definition_ids(&self) -> Result<HashSet<NodeId>, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(NODES).map_err(|e| e.to_string())?;
        let mut out = HashSet::new();
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (key, _) = entry.map_err(|e| e.to_string())?;
            out.insert(*key.value());
        }
        Ok(out)
    }

    /// Tally every reference row into per-language counts.
    pub fn report(&self) -> Result<Report, String> {
        let txn = self.db.begin_read().map_err(|e| e.to_string())?;
        let table = txn.open_table(REFS).map_err(|e| e.to_string())?;
        let mut report = Report::default();
        for entry in table.iter().map_err(|e| e.to_string())? {
            let (_, value) = entry.map_err(|e| e.to_string())?;
            let (record, _): (RefRecord, usize) =
                bincode::decode_from_slice(value.value(), config::standard())
                    .map_err(|e| e.to_string())?;
            let tally = report.per_lang.entry(record.lang).or_default();
            match record.outcome {
                StoredOutcome::Resolved(_) => tally.resolved += u64::from(record.count),
                StoredOutcome::External(_) => tally.external += u64::from(record.count),
                StoredOutcome::Unresolved(reason) => {
                    *tally.unresolved.entry(reason).or_default() += u64::from(record.count);
                }
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Lang, node_id};

    fn open_temp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("graph.redb")).expect("open");
        (dir, store)
    }

    #[test]
    fn batch_round_trips() {
        let (_dir, store) = open_temp();
        let def = node_id(Lang::Go, "m/pkg.Foo");
        let caller = node_id(Lang::Go, "m/pkg.Bar");
        let batch = Batch {
            files: vec![("pkg/a.go".into(), [7u8; 32])],
            nodes: vec![(
                def,
                NodeRecord::Definition {
                    fqn: "m/pkg.Foo".into(),
                    kind: 0,
                    file: "pkg/a.go".into(),
                    line: 3,
                },
            )],
            refs: vec![(
                "pkg/b.go".into(),
                0,
                "Foo".into(),
                RefRecord {
                    outcome: StoredOutcome::Resolved(def),
                    count: 4,
                    first_line: 9,
                    lang: Lang::Go.code(),
                },
            )],
            edges: vec![(caller, def, 0)],
            candidates: vec![(def, ("pkg/b.go".into(), 0, "Foo".into()))],
        };
        store.apply(&batch).expect("apply");
        assert_eq!(store.file_hash("pkg/a.go").unwrap(), Some([7u8; 32]));
        assert_eq!(store.file_hash("missing.go").unwrap(), None);
        assert!(store.definition_ids().unwrap().contains(&def));
    }

    #[test]
    fn report_sums_counts_by_language_and_reason() {
        let (_dir, store) = open_temp();
        let def = node_id(Lang::Go, "m/pkg.Foo");
        let rec = |outcome, count| RefRecord {
            outcome,
            count,
            first_line: 1,
            lang: Lang::Go.code(),
        };
        let batch = Batch {
            refs: vec![
                (
                    "a.go".into(),
                    0,
                    "Foo".into(),
                    rec(StoredOutcome::Resolved(def), 3),
                ),
                (
                    "a.go".into(),
                    0,
                    "x.Close".into(),
                    rec(StoredOutcome::Unresolved(5), 2),
                ),
                (
                    "b.go".into(),
                    1,
                    "fmt".into(),
                    rec(StoredOutcome::External("std:fmt".into()), 1),
                ),
            ],
            ..Batch::default()
        };
        store.apply(&batch).expect("apply");
        let report = store.report().expect("report");
        let go = &report.per_lang[&Lang::Go.code()];
        assert_eq!(go.resolved, 3);
        assert_eq!(go.external, 1);
        assert_eq!(go.unresolved_total(), 2);
        assert_eq!(go.unresolved[&5], 2);
        assert_eq!(
            crate::resolution_rate(go.resolved, go.unresolved_total()),
            Some(0.6)
        );
    }
}
