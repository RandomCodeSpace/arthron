//! Collision disposition is durable graph state, not an event-local tally
//! correction.

use std::collections::HashMap;
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
use arthron::store::{NodeRecord, Store};
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
