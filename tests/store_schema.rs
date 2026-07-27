//! The store's schema contract, exercised without a scan: version fencing,
//! per-file ownership, and the encoding of a reference row key.

use std::path::Path;

use arthron::model::{Domain, Lang, NodeId, node_id};
use arthron::store::{
    DeclSite, DefBatch, FileDefs, FileRefs, NodePayload, NodeRecord, RefBatch, RefKey, RefRecord,
    SCHEMA_VERSION, Store, StoredOutcome,
};
use redb::{Database, TableDefinition};

/// The metadata table, restated so a test can stamp a foreign generation on
/// a store the way an older build would have left one. Renaming the table in
/// `store.rs` makes this test fail loudly rather than quietly stop testing.
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

fn site(file: &str, line: u32) -> DeclSite {
    site_of(file, line, NodePayload::Definition(0))
}

fn site_of(file: &str, line: u32, payload: NodePayload) -> DeclSite {
    DeclSite {
        file: file.to_string(),
        line,
        payload,
    }
}

/// The site [`package`] declares, for asserting on what a package node kept.
fn pkg_site(file: &str) -> DeclSite {
    site_of(file, 1, NodePayload::Package(Some("pkg".to_string())))
}

fn go(fqn: &str) -> NodeId {
    node_id(Domain::Go, fqn)
}

fn definition(fqn: &str, file: &str, line: u32) -> (NodeId, NodeRecord) {
    (
        go(fqn),
        NodeRecord::Definition {
            fqn: fqn.to_string(),
            kind: 0,
            declarations: vec![site(file, line)],
        },
    )
}

fn package(import_path: &str, file: &str) -> (NodeId, NodeRecord) {
    // The site carries what this file declared, and the record's own name is
    // re-derived from its sites — so a package fixture whose site claimed a
    // definition would come back out of the store with no name at all.
    let name = "pkg".to_string();
    (
        go(import_path),
        NodeRecord::Package {
            import_path: import_path.to_string(),
            name: Some(name.clone()),
            declarations: vec![site_of(file, 1, NodePayload::Package(Some(name)))],
        },
    )
}

fn key(file: &str, enclosing: &str, raw: &str) -> RefKey {
    RefKey {
        file: file.to_string(),
        kind: 0,
        space: 0,
        enclosing: enclosing.to_string(),
        raw_target: raw.to_string(),
        argc: None,
        locally_bound: false,
    }
}

fn record(outcome: StoredOutcome) -> RefRecord {
    RefRecord {
        outcome,
        count: 1,
        first_line: 7,
        lang: Lang::Go.code(),
    }
}

/// One file's phase-2 half: one row, one edge, one candidate entry.
fn refs_of(file: &str, raw: &str, target: NodeId) -> FileRefs {
    let row = key(file, "m/pkg#Caller", raw);
    FileRefs {
        path: file.to_string(),
        hash: [1u8; 32],
        nodes: vec![],
        rows: vec![(row.clone(), record(StoredOutcome::Resolved(target)))],
        edges: vec![(go("m/pkg#Caller"), target, 0)],
        candidates: vec![(target, row)],
    }
}

fn open(path: &Path) -> Store {
    Store::open(path).expect("open")
}

#[test]
fn a_version_mismatch_forces_a_cold_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.redb");
    {
        let store = open(&path);
        store
            .apply_defs(&DefBatch {
                files: vec![FileDefs {
                    path: "pkg/a.go".into(),
                    nodes: vec![definition("m/pkg#Foo", "pkg/a.go", 3)],
                }],
            })
            .expect("apply defs");
        store
            .apply_refs(&RefBatch {
                files: vec![refs_of("pkg/b.go", "Foo", go("m/pkg#Foo"))],
            })
            .expect("apply refs");
        assert_eq!(store.known_files().unwrap(), ["pkg/b.go"]);
    }

    // A store written under any other generation is dropped, not migrated:
    // a graph is a cache of facts the source tree can always rebuild, and a
    // half-migrated one is worse than an absent one.
    let db = Database::create(&path).expect("raw open");
    let txn = db.begin_write().expect("write txn");
    {
        let mut meta = txn.open_table(META).expect("meta");
        meta.insert("schema_version", &(SCHEMA_VERSION - 1).to_le_bytes()[..])
            .expect("stamp");
    }
    txn.commit().expect("commit");
    drop(db);

    let store = open(&path);
    assert!(store.known_files().unwrap().is_empty());
    let snapshot = store.snapshot().expect("snapshot");
    assert!(snapshot.files.is_empty());
    assert!(snapshot.nodes.is_empty());
    assert!(snapshot.rows.is_empty());
    assert!(snapshot.edges.is_empty());
    assert!(snapshot.candidates.is_empty());
    assert!(store.report().unwrap().per_lang.is_empty());
}

#[test]
fn reopening_a_store_of_the_same_version_keeps_everything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.redb");
    let before = {
        let store = open(&path);
        store
            .apply_defs(&DefBatch {
                files: vec![FileDefs {
                    path: "pkg/a.go".into(),
                    nodes: vec![definition("m/pkg#Foo", "pkg/a.go", 3)],
                }],
            })
            .expect("apply defs");
        store
            .apply_refs(&RefBatch {
                files: vec![refs_of("pkg/b.go", "Foo", go("m/pkg#Foo"))],
            })
            .expect("apply refs");
        store.snapshot().expect("snapshot")
    };
    assert_eq!(open(&path).snapshot().expect("snapshot"), before);
}

#[test]
fn a_manifest_fingerprint_fences_the_store() {
    // The manifest decides every identity in the graph but is not one of
    // the files the walk hashes, so the store fences on a fingerprint of it.
    // Both directions matter: a changed fingerprint must wipe, and an
    // unchanged one must not — a fence that wiped every time would make
    // every scan cold, which no cold-versus-warm comparison can detect.
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));

    assert!(
        store.fence_config(Lang::Go, b"module-a").expect("fence"),
        "the first fingerprint a store sees is a change",
    );
    store
        .apply_refs(&RefBatch {
            files: vec![refs_of("pkg/b.go", "Foo", go("m/pkg#Foo"))],
        })
        .expect("apply refs");
    assert_eq!(store.known_files().unwrap(), ["pkg/b.go"]);

    assert!(
        !store.fence_config(Lang::Go, b"module-a").expect("fence"),
        "the same fingerprint is not an event",
    );
    assert_eq!(
        store.known_files().unwrap(),
        ["pkg/b.go"],
        "an unchanged manifest leaves the graph alone",
    );

    assert!(
        store.fence_config(Lang::Go, b"module-b").expect("fence"),
        "a different manifest describes a different project",
    );
    assert!(store.known_files().unwrap().is_empty());
    let snapshot = store.snapshot().expect("snapshot");
    assert!(snapshot.rows.is_empty());
    assert!(snapshot.edges.is_empty());
    assert!(snapshot.candidates.is_empty());
}

#[test]
fn replacing_a_file_removes_exactly_its_own_facts() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));
    store
        .apply_refs(&RefBatch {
            files: vec![
                refs_of("pkg/b.go", "Foo", go("m/pkg#Foo")),
                refs_of("pkg/c.go", "Bar", go("m/pkg#Bar")),
            ],
        })
        .expect("apply refs");
    let before = store.snapshot().expect("snapshot");

    // Re-apply `b.go` with a different reference. Everything `c.go` owns
    // must come out byte-identical.
    store
        .apply_refs(&RefBatch {
            files: vec![refs_of("pkg/b.go", "Renamed", go("m/pkg#Renamed"))],
        })
        .expect("replace");
    let after = store.snapshot().expect("snapshot");

    let untouched = key("pkg/c.go", "m/pkg#Caller", "Bar");
    assert_eq!(after.rows.get(&untouched), before.rows.get(&untouched));
    assert!(
        after
            .edges
            .contains(&(go("m/pkg#Caller"), go("m/pkg#Bar"), 0))
    );
    assert_eq!(
        after.candidates.get(&go("m/pkg#Bar")),
        before.candidates.get(&go("m/pkg#Bar")),
    );

    // And nothing of the old `b.go` survives.
    assert!(
        !after
            .rows
            .contains_key(&key("pkg/b.go", "m/pkg#Caller", "Foo"))
    );
    assert!(
        !after
            .edges
            .contains(&(go("m/pkg#Caller"), go("m/pkg#Foo"), 0))
    );
    assert!(!after.candidates.contains_key(&go("m/pkg#Foo")));
    assert!(
        after
            .rows
            .contains_key(&key("pkg/b.go", "m/pkg#Caller", "Renamed"))
    );
}

#[test]
fn a_row_key_identical_but_for_its_file_is_a_different_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));
    store
        .apply_refs(&RefBatch {
            files: vec![
                refs_of("pkg/b.go", "Foo", go("m/pkg#Foo")),
                refs_of("pkg/c.go", "Foo", go("m/pkg#Foo")),
            ],
        })
        .expect("apply refs");
    let snapshot = store.snapshot().expect("snapshot");
    assert_eq!(snapshot.rows.len(), 2, "{:?}", snapshot.rows.keys());
    assert_eq!(
        snapshot.candidates[&go("m/pkg#Foo")].len(),
        2,
        "both files probed the identity, so both must be woken by it"
    );

    // Replacing one leaves the other's row and candidate entry alone.
    store
        .apply_refs(&RefBatch {
            files: vec![FileRefs {
                path: "pkg/b.go".into(),
                hash: [2u8; 32],
                ..FileRefs::default()
            }],
        })
        .expect("replace");
    let after = store.snapshot().expect("snapshot");
    assert_eq!(after.rows.len(), 1);
    assert!(
        after
            .rows
            .contains_key(&key("pkg/c.go", "m/pkg#Caller", "Foo"))
    );
    assert_eq!(after.candidates[&go("m/pkg#Foo")].len(), 1);
}

#[test]
fn a_node_two_files_declare_survives_one_of_them_being_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));
    store
        .apply_defs(&DefBatch {
            files: vec![
                FileDefs {
                    path: "pkg/a.go".into(),
                    nodes: vec![package("m/pkg", "pkg/a.go")],
                },
                FileDefs {
                    path: "pkg/b.go".into(),
                    nodes: vec![package("m/pkg", "pkg/b.go")],
                },
            ],
        })
        .expect("apply defs");
    let record = store.node(&go("m/pkg")).unwrap().expect("the package node");
    assert_eq!(
        record.declarations(),
        [pkg_site("pkg/a.go"), pkg_site("pkg/b.go")],
    );

    store.forget_files(&["pkg/a.go".into()]).expect("forget");
    let record = store
        .node(&go("m/pkg"))
        .unwrap()
        .expect("a package one file still declares is still a node");
    assert_eq!(record.declarations(), [pkg_site("pkg/b.go")]);

    // The last declaration goes, and the node with it.
    store.forget_files(&["pkg/b.go".into()]).expect("forget");
    assert_eq!(store.node(&go("m/pkg")).unwrap(), None);
}

#[test]
fn candidate_entries_are_removed_with_their_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));
    store
        .apply_refs(&RefBatch {
            files: vec![refs_of("pkg/b.go", "Foo", go("m/pkg#Foo"))],
        })
        .expect("apply refs");
    assert_eq!(
        store.candidate_rows(&go("m/pkg#Foo")).unwrap(),
        [key("pkg/b.go", "m/pkg#Caller", "Foo")],
    );

    // The bug this closes: a candidate entry outliving the row that probed
    // it points invalidation at a reference that no longer exists.
    store.forget_files(&["pkg/b.go".into()]).expect("forget");
    assert!(store.candidate_rows(&go("m/pkg#Foo")).unwrap().is_empty());
    assert!(store.snapshot().unwrap().candidates.is_empty());
}

#[test]
fn an_external_node_many_files_reach_keeps_one_site_per_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));
    let id = go("external:std:fmt");
    let external = |file: &str, line: u32| FileRefs {
        path: file.to_string(),
        hash: [3u8; 32],
        nodes: vec![(
            id,
            NodeRecord::External {
                package: "std:fmt".into(),
                declarations: vec![site(file, line)],
            },
        )],
        ..FileRefs::default()
    };
    // Applied out of order on purpose: the stored sites are sorted, so a
    // snapshot cannot depend on which file was written first.
    store
        .apply_refs(&RefBatch {
            files: (0..40)
                .rev()
                .map(|i| external(&format!("pkg/f{i:02}.go"), i + 1))
                .collect(),
        })
        .expect("apply refs");
    let record = store.node(&id).unwrap().expect("the external node");
    assert_eq!(record.declarations().len(), 40);
    assert_eq!(record.declarations()[0], site("pkg/f00.go", 1));
    assert_eq!(record.declarations()[39], site("pkg/f39.go", 40));

    store
        .forget_files(&["pkg/f00.go".to_string()])
        .expect("forget");
    let record = store.node(&id).unwrap().expect("39 files still reach it");
    assert_eq!(record.declarations().len(), 39);
}

#[test]
fn a_file_with_no_facts_still_gets_a_hash_and_an_owned_record() {
    // Without one it would be re-extracted on every scan, forever.
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));
    store
        .apply_refs(&RefBatch {
            files: vec![FileRefs {
                path: "pkg/empty.go".into(),
                hash: [9u8; 32],
                ..FileRefs::default()
            }],
        })
        .expect("apply refs");
    assert_eq!(store.file_hash("pkg/empty.go").unwrap(), Some([9u8; 32]));
    assert_eq!(store.known_files().unwrap(), ["pkg/empty.go"]);
}

#[test]
fn applying_a_ref_half_without_its_def_half_does_not_corrupt_the_store() {
    // Should not happen — the driver runs phase 1 first. If it ever does,
    // the store must stay readable rather than half-written.
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));
    store
        .apply_refs(&RefBatch {
            files: vec![refs_of("pkg/b.go", "Foo", go("m/pkg#Foo"))],
        })
        .expect("apply refs");
    let snapshot = store.snapshot().expect("snapshot");
    assert_eq!(snapshot.rows.len(), 1);
    assert!(
        snapshot.nodes.is_empty(),
        "an edge may point at a node no file declares; it does not invent one"
    );
    assert_eq!(store.report().unwrap().fqn_collisions, 0);
}

#[test]
fn row_keys_round_trip_through_the_split_encoding() {
    let cases = [
        key("pkg/a.go", "m/pkg#Caller", "Foo"),
        RefKey {
            argc: Some(0),
            ..key("pkg/a.go", "m/pkg#Caller", "Foo")
        },
        RefKey {
            argc: Some(u32::MAX),
            ..key("pkg/a.go", "m/pkg#Caller", "Foo")
        },
        // No nameable encloser and no container: an empty string, not a
        // missing component.
        key("pkg/a.go", "", "Foo"),
        // A target carrying the separators a hand-rolled encoding would
        // have tripped over.
        key("pkg/a.go", "m/pkg#Caller", "h.reset().apply"),
        key("pkg/a.go", "m/pkg#Caller", "a/b\u{0}c.d"),
        RefKey {
            kind: 9,
            space: 2,
            ..key("pkg/a.go", "m/pkg#C.m", "x")
        },
        RefKey {
            locally_bound: true,
            ..key("pkg/a.go", "m/pkg#Caller", "x")
        },
    ];
    for original in cases {
        let (file, encoded) = original.split();
        let rebuilt = RefKey::join(file, &encoded).expect("a split key rejoins");
        assert_eq!(rebuilt, original);
    }

    // `None` and `Some(0)` are different arities, so they must be different
    // keys — collapsing them would merge two call sites into one row.
    let unknown = key("pkg/a.go", "m/pkg#Caller", "Foo");
    let zero = RefKey {
        argc: Some(0),
        ..unknown.clone()
    };
    assert_ne!(unknown.split().1, zero.split().1);

    // A block-local `x()` and the package-level `x()` after it agree on
    // every other component and resolve differently, so they must be
    // different keys — one row carries one outcome.
    let bound = RefKey {
        locally_bound: true,
        ..unknown.clone()
    };
    assert_ne!(unknown.split().1, bound.split().1);

    // Trailing bytes are an error: an encoding that accepts two byte
    // strings for one key is not a key.
    let (_, mut encoded) = unknown.split();
    encoded.push(0);
    assert!(RefKey::join("pkg/a.go", &encoded).is_err());
    assert!(RefKey::join("pkg/a.go", &[]).is_err());
}

#[test]
fn an_edge_two_files_produce_survives_one_of_them_being_forgotten() {
    // An edge is keyed by `(src, dst, kind)` and nothing else, so two files
    // can produce the very same triple: two files of one package whose
    // package-level references both reach the same target — every file in a
    // package importing the same package, for instance. Removing one file's
    // claim unconditionally used to delete the other's edge too.
    //
    // Nothing in the report notices: tallies are summed from per-file rows,
    // never from the edge table. Only a whole-store comparison against a
    // cold scan sees it, which is why the rule lives here, where it is one
    // assertion instead of a corpus.
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));
    let shared = (go("m/pkg#Caller"), go("m/pkg#Foo"), 0);
    store
        .apply_refs(&RefBatch {
            files: vec![
                refs_of("pkg/b.go", "Foo", go("m/pkg#Foo")),
                refs_of("pkg/c.go", "Foo", go("m/pkg#Foo")),
            ],
        })
        .expect("apply refs");
    assert!(store.snapshot().unwrap().edges.contains(&shared));

    // Replacing one producer's half leaves the other's claim standing.
    store
        .apply_refs(&RefBatch {
            files: vec![refs_of("pkg/b.go", "Foo", go("m/pkg#Foo"))],
        })
        .expect("replace");
    assert!(store.snapshot().unwrap().edges.contains(&shared));

    // So does forgetting it outright.
    store
        .forget_files(&["pkg/b.go".to_string()])
        .expect("forget");
    assert!(
        store.snapshot().unwrap().edges.contains(&shared),
        "`pkg/c.go` still produces this edge",
    );
    assert!(store.has_edge(&shared.0, &shared.1, shared.2).unwrap());

    // And the last producer going takes it, leaving nothing behind.
    store
        .forget_files(&["pkg/c.go".to_string()])
        .expect("forget");
    let snapshot = store.snapshot().unwrap();
    assert!(!snapshot.edges.contains(&shared), "nothing produces it now");
    assert!(snapshot.edges.is_empty());
    assert!(!store.has_edge(&shared.0, &shared.1, shared.2).unwrap());
}

#[test]
fn one_languages_fence_does_not_wipe_anothers_rows() {
    // Two live languages share one store, and a language's manifest says
    // nothing about the other's graph. The global fence made every second
    // language's scan wipe the first's rows — the exact bug this pins.
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));

    assert!(store.fence_config(Lang::Go, b"module-a").expect("fence"));
    store
        .apply_refs(&RefBatch {
            files: vec![refs_of("pkg/b.go", "Foo", go("m/pkg#Foo"))],
        })
        .expect("apply refs");

    // A manifest-less language fences with an empty digest: no opinion,
    // nothing invalidated, nothing stored.
    assert!(
        !store.fence_config(Lang::Java, b"").expect("fence"),
        "an empty digest is no opinion",
    );
    assert_eq!(store.known_files().unwrap(), ["pkg/b.go"]);

    // Even a manifest-bearing second language wipes only its own files.
    assert!(
        store
            .fence_config(Lang::Java, b"pom-digest")
            .expect("fence")
    );
    assert_eq!(
        store.known_files().unwrap(),
        ["pkg/b.go"],
        "Java's fence forgot Go's rows",
    );
    assert!(
        !store.fence_config(Lang::Go, b"module-a").expect("fence"),
        "Go's own fingerprint survived Java's fence",
    );
}

#[test]
fn a_stale_fence_forgets_only_that_languages_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir.path().join("graph.redb"));

    assert!(store.fence_config(Lang::Go, b"module-a").expect("fence"));
    store
        .apply_refs(&RefBatch {
            files: vec![
                refs_of("pkg/b.go", "Foo", go("m/pkg#Foo")),
                refs_of("src/A.java", "Foo", go("j/pkg#Foo")),
            ],
        })
        .expect("apply refs");

    assert!(
        store.fence_config(Lang::Go, b"module-b").expect("fence"),
        "a different manifest describes a different project",
    );
    assert_eq!(
        store.known_files().unwrap(),
        ["src/A.java"],
        "the Go fence reached beyond Go's own files",
    );
}
