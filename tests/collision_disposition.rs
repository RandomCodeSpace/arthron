//! Collision disposition is durable graph state, not an event-local tally
//! correction.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use arthron::Outcome;
use arthron::lang::{
    Extractor, FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe,
};
use arthron::model::{
    DeclSpace, DefFacets, DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, Reference, Span,
    node_id,
};
use arthron::pipeline::{scan, scan_repo};
use arthron::store::{
    CollisionDisposition, DeclSite, DefBatch, FileDefs, NodePayload, NodeRecord, Store,
    StoredDefinition,
};
use arthron::track_csharp::resolve::scan_csharp;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, body).expect("write fixture");
}

fn declaration_paths(store: &Store, domain: Domain, fqn: &str) -> Vec<String> {
    let id = node_id(domain, fqn);
    let record = store
        .node(&id)
        .expect("node lookup")
        .unwrap_or_else(|| panic!("{fqn} is stored"));
    let NodeRecord::Definition {
        fqn: stored,
        declarations,
        ..
    } = record
    else {
        panic!("{fqn} is a definition");
    };
    assert_eq!(stored, fqn);
    declarations.into_iter().map(|site| site.file).collect()
}

fn withdrawn_claims(store: &Store) -> usize {
    store
        .snapshot()
        .expect("snapshot")
        .files
        .values()
        .filter(|hash| hash.is_none())
        .count()
}

fn partial_type_tree(root: &Path, reverse_creation: bool) {
    let declarations = [
        ("a.cs", "namespace N;\npublic partial class Shared {}\n"),
        ("z.cs", "namespace N;\npublic partial class Shared {}\n"),
    ];
    if reverse_creation {
        for (rel, body) in declarations.into_iter().rev() {
            write(root, rel, body);
        }
    } else {
        for (rel, body) in declarations {
            write(root, rel, body);
        }
    }
}

#[test]
fn n_shared_keeps_both_declarations_and_is_mergeable_cold_warm_store_and_registry() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path().join("direct");
    std::fs::create_dir(&root).expect("direct root");
    partial_type_tree(&root, true);
    let db = scratch.path().join("direct.redb");

    let cold = scan_csharp(&root, &db).expect("cold C# scan");
    assert_eq!(cold.fqn_collisions, 0, "cold N#Shared is one partial type");
    let warm = scan_csharp(&root, &db).expect("unchanged warm C# scan");
    assert_eq!(
        warm.fqn_collisions, 0,
        "unchanged warm N#Shared keeps the cold disposition",
    );

    let store = Store::open(&db).expect("direct store opens");
    assert_eq!(
        declaration_paths(&store, Domain::CSharp, "N#Shared"),
        ["a.cs", "z.cs"],
        "both partial declaration sites remain queryable by the exact FQN",
    );
    assert_eq!(
        store.report().expect("direct store report").fqn_collisions,
        0,
        "Store::report owns the durable mergeable disposition",
    );
    drop(store);

    let registry_root = scratch.path().join("registry");
    std::fs::create_dir(&registry_root).expect("registry root");
    partial_type_tree(&registry_root, false);
    let registry_db = scratch.path().join("registry.redb");
    let registry = scan_repo(&registry_root, &registry_db).expect("full registry scan");
    assert_eq!(
        registry.fqn_collisions, 0,
        "a later registry track cannot resurrect N#Shared as a collision",
    );
    let registry_store = Store::open(&registry_db).expect("registry store opens");
    assert_eq!(
        declaration_paths(&registry_store, Domain::CSharp, "N#Shared"),
        ["a.cs", "z.cs"],
    );
}

#[test]
fn n_shared_value_is_a_named_nonmergeable_collision_cold_and_warm() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    write(
        root,
        "a.cs",
        "namespace N;\npublic partial class Shared { public int Value; }\n",
    );
    write(
        root,
        "z.cs",
        "namespace N;\npublic partial class Shared { public int Value { get; } }\n",
    );
    let db = root.join("graph.redb");

    let cold = scan_csharp(root, &db).expect("cold C# scan");
    assert_eq!(
        cold.fqn_collisions, 1,
        "N#Shared::Value is a field and a property, not one declaration",
    );
    let warm = scan_csharp(root, &db).expect("unchanged warm C# scan");
    assert_eq!(
        warm.fqn_collisions, 1,
        "the named nonmergeable collision is stable when no file changed",
    );
    let store = Store::open(&db).expect("store opens");
    assert_eq!(
        declaration_paths(&store, Domain::CSharp, "N#Shared::Value"),
        ["a.cs", "z.cs"],
        "neither half of the collision is discarded",
    );
    assert_eq!(store.report().expect("store report").fqn_collisions, 1);
}

struct PairwiseLang;

impl Language for PairwiseLang {
    const LANG: Lang = Lang::CSharp;
    const DOMAIN: Domain = Domain::CSharp;

    fn extensions() -> &'static [&'static str] {
        &["collision"]
    }

    type Header = ();
    type Scope = ();
    type Config = ();
}

struct PairwiseExtractor;

impl Extractor<PairwiseLang> for PairwiseExtractor {
    fn extract(&self, _rel_path: &str, source: &str) -> FileFacts<PairwiseLang> {
        let kind = match source.trim() {
            "field" => DefKind::Field,
            "property" => DefKind::Property,
            other => panic!("unknown declaration shape {other}"),
        };
        FileFacts {
            header: (),
            defs: vec![Definition {
                kind,
                name: "Value".to_string(),
                owner: vec!["N".to_string(), "Shared".to_string()],
                space: DeclSpace::Value,
                facets: DefFacets::default(),
                params: None,
                span: Span {
                    byte_start: 0,
                    byte_end: source.len().try_into().expect("tiny fixture"),
                    line: 1,
                },
            }],
            refs: Vec::new(),
        }
    }
}

struct PairwiseResolver;

impl Resolver<PairwiseLang> for PairwiseResolver {
    fn config(&self, _root: &Path, _files: &FileIndex) -> Result<(), LayoutError> {
        Ok(())
    }

    fn config_digest(&self, _cfg: &()) -> Vec<u8> {
        Vec::new()
    }

    fn declared_container(&self, _cfg: &(), _header: &()) -> Option<(String, String)> {
        None
    }

    fn learn_containers(&self, _cfg: &mut (), _names: &HashMap<String, String>) {}

    fn owns_file(&self, _cfg: &(), _rel_path: &str) -> bool {
        true
    }

    fn def_fqn(
        &self,
        _cfg: &(),
        _header: &(),
        _owner: &[String],
        _def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        Some(Fqn::new("N#Shared::Value"))
    }

    fn index_keys(&self, _cfg: &(), _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        Vec::new()
    }

    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        matches!(
            (a.kind, b.kind),
            (DefKind::Field, DefKind::Property)
                | (DefKind::Property, DefKind::Field)
                | (DefKind::Property, DefKind::Property)
        ) && a.name == b.name
            && a.owner == b.owner
    }

    fn scope(&self, _cfg: &(), _file: &FileFacts<PairwiseLang>, _probe: &dyn SymbolProbe) {}

    fn link_kinds(&self) -> &'static [RefKind] {
        &[]
    }

    fn resolve(
        &self,
        _cfg: &(),
        _scope: &(),
        _reference: &Reference,
        _probe: &dyn SymbolProbe,
    ) -> Resolution {
        Resolution {
            outcome: Outcome::Unresolved(arthron::UnresolvedReason::TierTwoLanguage),
            candidates: Vec::new(),
        }
    }
}

fn pairwise_node(file: &str, shape: &str) -> (NodeId, NodeRecord) {
    let facts = PairwiseExtractor.extract(file, shape);
    let definition = &facts.defs[0];
    let id = node_id(Domain::CSharp, "N#Shared::Value");
    (
        id,
        NodeRecord::Definition {
            fqn: "N#Shared::Value".to_string(),
            kind: definition.kind.code(),
            facets: definition.facets.bits(),
            targets: Vec::new(),
            declarations: vec![DeclSite {
                file: file.to_string(),
                line: definition.span.line,
                payload: NodePayload::Definition(definition.kind.code(), definition.facets.bits()),
                merge_definition: Some(StoredDefinition::from_definition(definition)),
            }],
        },
    )
}

#[test]
fn three_declarations_check_every_pair_not_only_adjacent_windows() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    write(root, "a.collision", "field");
    write(root, "b.collision", "property");
    write(root, "c.collision", "field");
    let db = root.join("graph.redb");

    let cold = scan::<PairwiseLang>(
        root,
        &db,
        &PairwiseExtractor,
        &PairwiseResolver,
        &arthron::config::FileFilter::none(),
    )
    .expect("cold pairwise scan");
    assert_eq!(
        cold.fqn_collisions, 1,
        "the non-adjacent field pair makes N#Shared::Value nonmergeable",
    );
    let warm = scan::<PairwiseLang>(
        root,
        &db,
        &PairwiseExtractor,
        &PairwiseResolver,
        &arthron::config::FileFilter::none(),
    )
    .expect("unchanged warm pairwise scan");
    assert_eq!(warm.fqn_collisions, 1);
    let store = Store::open(&db).expect("store opens");
    assert_eq!(
        declaration_paths(&store, Domain::CSharp, "N#Shared::Value"),
        ["a.collision", "b.collision", "c.collision"],
    );
    assert_eq!(store.report().expect("store report").fqn_collisions, 1);
    drop(store);

    std::fs::remove_file(root.join("c.collision")).expect("remove incompatible field");
    let after_delete = scan::<PairwiseLang>(
        root,
        &db,
        &PairwiseExtractor,
        &PairwiseResolver,
        &arthron::config::FileFilter::none(),
    )
    .expect("incremental deletion scan");
    assert_eq!(
        after_delete.fqn_collisions, 0,
        "removing the incompatible declaration durably reclassifies the surviving pair",
    );
    let store = Store::open(&db).expect("store reopens");
    assert_eq!(
        declaration_paths(&store, Domain::CSharp, "N#Shared::Value"),
        ["a.collision", "b.collision"],
    );
    assert_eq!(
        store.report().expect("post-delete report").fqn_collisions,
        0
    );
}

#[test]
fn deleting_the_middle_declaration_withdraws_the_old_disposition_until_the_survivors_recompute() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    write(root, "a.collision", "field");
    write(root, "b.collision", "property");
    write(root, "c.collision", "field");
    let db = root.join("graph.redb");

    let cold = scan::<PairwiseLang>(
        root,
        &db,
        &PairwiseExtractor,
        &PairwiseResolver,
        &arthron::config::FileFilter::none(),
    )
    .expect("cold pairwise scan");
    assert_eq!(cold.fqn_collisions, 1, "the field pair is incompatible");

    std::fs::remove_file(root.join("b.collision")).expect("remove middle property");
    let id = node_id(Domain::CSharp, "N#Shared::Value");
    let store = Store::open(&db).expect("store opens");
    store
        .forget_files(&["b.collision".to_string()])
        .expect("forget deleted declaration");

    assert_eq!(
        store.report().expect("interrupted report").fqn_collisions,
        0,
        "an interrupted deletion must not publish the prior collision verdict",
    );
    assert_eq!(
        withdrawn_claims(&store),
        2,
        "both surviving declaration files must be re-scanned before a verdict is published",
    );
    assert!(
        !store
            .snapshot()
            .expect("interrupted snapshot")
            .collision_dispositions
            .contains_key(&id),
        "the old verdict describes a declaration set that no longer exists",
    );
    drop(store);

    let resumed = scan::<PairwiseLang>(
        root,
        &db,
        &PairwiseExtractor,
        &PairwiseResolver,
        &arthron::config::FileFilter::none(),
    )
    .expect("resumed pairwise scan");
    assert_eq!(
        resumed.fqn_collisions, 1,
        "the recomputed field/field pair remains a collision; no Mergeable verdict is invented",
    );
    let settled = Store::open(&db).expect("settled store opens");
    assert_eq!(withdrawn_claims(&settled), 0);
    assert_eq!(
        settled
            .snapshot()
            .expect("settled snapshot")
            .collision_dispositions
            .get(&id),
        Some(&CollisionDisposition::Collision),
        "only the resolver may restore the collision verdict for the surviving set",
    );
}

#[test]
fn replacing_one_declaration_withdraws_the_old_disposition_until_phase_two_finishes() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    write(root, "a.collision", "field");
    write(root, "b.collision", "property");
    let db = root.join("graph.redb");

    let cold = scan::<PairwiseLang>(
        root,
        &db,
        &PairwiseExtractor,
        &PairwiseResolver,
        &arthron::config::FileFilter::none(),
    )
    .expect("cold pairwise scan");
    assert_eq!(cold.fqn_collisions, 0, "field/property is mergeable");

    write(root, "b.collision", "field");
    let id = node_id(Domain::CSharp, "N#Shared::Value");
    let store = Store::open(&db).expect("store opens");
    store
        .apply_defs(&DefBatch {
            files: vec![FileDefs {
                path: "b.collision".to_string(),
                nodes: vec![pairwise_node("b.collision", "field")],
            }],
        })
        .expect("replace the definition half");

    assert_eq!(
        store.report().expect("interrupted report").fqn_collisions,
        0,
        "the old Mergeable verdict must not stand in for the changed pair",
    );
    assert_eq!(withdrawn_claims(&store), 2);
    assert!(
        !store
            .snapshot()
            .expect("interrupted snapshot")
            .collision_dispositions
            .contains_key(&id),
        "the old Mergeable verdict described the pre-replacement set",
    );
    drop(store);

    let resumed = scan::<PairwiseLang>(
        root,
        &db,
        &PairwiseExtractor,
        &PairwiseResolver,
        &arthron::config::FileFilter::none(),
    )
    .expect("resumed pairwise scan");
    assert_eq!(
        resumed.fqn_collisions, 1,
        "the resolver, not Store::apply_defs, classifies the changed field/field pair",
    );
}

#[test]
fn removing_a_set_to_zero_does_not_suppress_a_later_fresh_direct_collision() {
    let scratch = tempfile::tempdir().expect("scratch");
    let store = Store::open(&scratch.path().join("graph.redb")).expect("store opens");
    let id = node_id(Domain::CSharp, "N#Shared::Value");
    let apply = |paths: &[&str]| {
        store
            .apply_defs(&DefBatch {
                files: paths
                    .iter()
                    .map(|path| FileDefs {
                        path: (*path).to_string(),
                        nodes: vec![pairwise_node(path, "field")],
                    })
                    .collect(),
            })
            .expect("apply definitions");
    };

    apply(&["a.collision", "b.collision"]);
    store
        .set_collision_dispositions(
            &BTreeSet::from([id]),
            &BTreeMap::from([(id, CollisionDisposition::Mergeable)]),
        )
        .expect("settle initial pair");
    store
        .forget_files(&["b.collision".to_string()])
        .expect("leave one declaration");
    apply(&["replacement-b.collision"]);
    assert_eq!(
        store
            .report()
            .expect("one-to-two direct report")
            .fqn_collisions,
        1,
        "a marker cannot survive when only one declaration was left",
    );
    store
        .set_collision_dispositions(
            &BTreeSet::from([id]),
            &BTreeMap::from([(id, CollisionDisposition::Mergeable)]),
        )
        .expect("settle replacement pair");
    store
        .forget_files(&[
            "replacement-b.collision".to_string(),
            "a.collision".to_string(),
        ])
        .expect("leave zero declarations");

    apply(&["fresh-a.collision", "fresh-b.collision"]);
    assert_eq!(
        store.report().expect("fresh direct report").fqn_collisions,
        1,
        "an invalidated verdict cannot outlive the declaration set it described",
    );
}
