//! Resolver fixtures for the Bash track: the import model, and the reason
//! every miss carries.
//!
//! `source` is the whole of the model. Bash resolves its argument against the
//! **working directory**, not against the sourcing file — a fact that decides
//! every rule below — and a specifier that is not a literal resolves against
//! nothing at all.

use std::collections::HashSet;

use arthron::UnresolvedReason;
use arthron::lang::Resolver;
use arthron::model::{
    DeclSpace, DefFacets, DefKind, Definition, Domain, Fqn, NodeId, Span, node_id,
};
use arthron::track_bash::extract::{BashHeader, extract};
use arthron::track_bash::lang::{BashProject, function_fqn, script_fqn};
use arthron::track_bash::resolve::BashResolver;

/// A symbol table holding exactly the scripts named.
fn tree(paths: &[&str]) -> HashSet<NodeId> {
    paths
        .iter()
        .map(|p| node_id(Domain::Shell, &script_fqn(p)))
        .collect()
}

/// Resolve the one `source` clause `body` writes, against `paths`.
fn outcome(body: &str, paths: &[&str]) -> arthron::Outcome<NodeId, String> {
    let facts = extract("lib/util.bash", body);
    let table = tree(paths);
    let scope = BashResolver.scope(&BashProject, &facts, &table);
    let r = facts.refs.first().expect("one reference");
    BashResolver
        .resolve(&BashProject, &scope, r, &table)
        .outcome
}

/// The candidates the one `source` clause `body` probes.
fn candidates(body: &str, paths: &[&str]) -> Vec<NodeId> {
    let facts = extract("lib/util.bash", body);
    let table = tree(paths);
    let scope = BashResolver.scope(&BashProject, &facts, &table);
    let r = facts.refs.first().expect("one reference");
    BashResolver
        .resolve(&BashProject, &scope, r, &table)
        .candidates
}

fn unresolved(body: &str, paths: &[&str]) -> UnresolvedReason {
    match outcome(body, paths) {
        arthron::Outcome::Unresolved(reason) => reason,
        other => panic!("{body:?} resolved: {other:?}"),
    }
}

#[test]
fn a_literal_path_is_anchored_at_the_repository_root() {
    // Bash resolves a `/`-carrying argument against the working directory,
    // and the repository root is the only working directory this scan can
    // name. It is deliberately *not* the sourcing file's directory: bash does
    // not look there, and probing it would resolve a reference the shell
    // itself would not.
    assert_eq!(
        outcome("source lib/other.bash\n", &["lib/other.bash"]),
        arthron::Outcome::Resolved(node_id(Domain::Shell, &script_fqn("lib/other.bash"))),
    );
    // The sourcing file sits in `lib/`; `other.bash` beside it is not what
    // this names.
    assert_eq!(
        unresolved("source other.bash\n", &["lib/other.bash"]),
        UnresolvedReason::UnknownPackage,
    );
}

#[test]
fn a_dot_prefixed_path_normalizes_against_the_root() {
    assert_eq!(
        outcome("source ./lib/other.bash\n", &["lib/other.bash"]),
        arthron::Outcome::Resolved(node_id(Domain::Shell, &script_fqn("lib/other.bash"))),
    );
    assert_eq!(
        outcome(". lib/../lib/other.bash\n", &["lib/other.bash"]),
        arthron::Outcome::Resolved(node_id(Domain::Shell, &script_fqn("lib/other.bash"))),
    );
}

#[test]
fn a_literal_inside_the_tree_that_names_nothing_is_module_not_found() {
    // The lookup was complete for the extensions this track owns, and the
    // literal named none of them.
    assert_eq!(
        unresolved("source lib/absent.bash\n", &["lib/other.bash"]),
        UnresolvedReason::ModuleNotFound,
    );
    // One candidate was really probed, and it is recorded — that is what
    // wakes this reference when the file it names appears.
    assert_eq!(
        candidates("source lib/absent.bash\n", &["lib/other.bash"]),
        [node_id(Domain::Shell, &script_fqn("lib/absent.bash"))],
    );
}

#[test]
fn a_path_that_leaves_the_repository_is_an_unindexed_package() {
    // Outside the repository by construction. Nothing in the tree can be
    // probed for it, so nothing is.
    for body in [
        "source /etc/profile\n",
        ". /usr/share/bash-completion/bash_completion\n",
        "source ../outside.bash\n",
        "source ./../outside.bash\n",
    ] {
        assert_eq!(
            unresolved(body, &["lib/other.bash"]),
            UnresolvedReason::UnknownPackage,
            "{body:?}",
        );
        assert!(candidates(body, &["lib/other.bash"]).is_empty(), "{body:?}");
    }
}

#[test]
fn a_bare_name_is_probed_at_the_root_and_then_belongs_to_path() {
    // Bash searches `$PATH` for an argument carrying no `/`, and falls back
    // to the working directory. The root is probed; `$PATH` is an environment
    // variable this build does not read and will not invent.
    assert_eq!(
        outcome("source util.bash\n", &["util.bash"]),
        arthron::Outcome::Resolved(node_id(Domain::Shell, &script_fqn("util.bash"))),
    );
    assert_eq!(
        unresolved("source util.bash\n", &["lib/util.bash"]),
        UnresolvedReason::UnknownPackage,
    );
}

#[test]
fn an_empty_specifier_names_no_file_and_is_not_guessed_at() {
    assert_eq!(
        unresolved("source \"\"\n", &["lib/other.bash"]),
        UnresolvedReason::ModuleNotFound,
    );
}

#[test]
fn a_computed_specifier_probes_nothing_and_is_never_guessed() {
    for body in [
        "source \"$BATS_ROOT/$BATS_LIBDIR/bats-core/other.bash\"\n",
        "source \"$1\"\n",
        "source $lib\n",
        "source \"$(dirname \"$0\")/other.bash\"\n",
    ] {
        assert_eq!(
            unresolved(body, &["lib/other.bash", "bats-core/other.bash"]),
            UnresolvedReason::DynamicModuleSpecifier,
            "{body:?}",
        );
        // The tail of the composed path really does name a file in this
        // tree. Matching it would be a guess about two environment variables
        // this build never reads, so no candidate is probed at all.
        assert!(candidates(body, &["lib/other.bash"]).is_empty(), "{body:?}");
    }
}

#[test]
fn this_track_mints_no_external_node() {
    // Bash has no manifest, so nothing in a repository *declares* that a name
    // comes from outside it. `External` sits outside both terms of the rate,
    // so a track that mints none cannot raise its rate by reclassifying —
    // every path that leaves the repository counts against it instead.
    for body in [
        "source /etc/profile\n",
        "source util.bash\n",
        "source ../outside.bash\n",
        "source \"$x\"\n",
    ] {
        assert!(
            !matches!(outcome(body, &[]), arthron::Outcome::External(_)),
            "{body:?}",
        );
    }
}

// -- the FQN grammar ------------------------------------------------------

fn header() -> BashHeader {
    BashHeader {
        rel_path: "lib/bats-core/common.bash".to_string(),
        sources: Vec::new(),
    }
}

fn def_of(kind: DefKind, name: &str, owner: &[&str], facets: DefFacets) -> Definition {
    Definition {
        kind,
        name: name.to_string(),
        owner: owner.iter().map(|s| (*s).to_string()).collect(),
        space: DeclSpace::Value,
        facets,
        params: None,
        span: Span {
            byte_start: 0,
            byte_end: 0,
            line: 1,
        },
    }
}

#[test]
fn a_script_is_named_by_its_path_and_a_function_by_its_file_and_chain() {
    let table: HashSet<NodeId> = HashSet::new();
    let script = def_of(DefKind::Module, "common.bash", &[], DefFacets::SYNTHETIC);
    assert_eq!(
        BashResolver
            .def_fqn(&BashProject, &header(), &[], &script, &table)
            .map(Fqn::into_string),
        Some("$lib/bats-core/common.bash".to_string()),
    );
    let f = def_of(DefKind::Function, "bats_trim", &[], DefFacets::default());
    assert_eq!(
        BashResolver
            .def_fqn(&BashProject, &header(), &[], &f, &table)
            .map(Fqn::into_string),
        Some("$lib/bats-core/common.bash#bats_trim".to_string()),
    );
    let nested = def_of(DefKind::Function, "inner", &["outer"], DefFacets::default());
    assert_eq!(
        BashResolver
            .def_fqn(
                &BashProject,
                &header(),
                &["outer".to_string()],
                &nested,
                &table
            )
            .map(Fqn::into_string),
        Some("$lib/bats-core/common.bash#outer.inner".to_string()),
    );
}

#[test]
fn two_files_declaring_one_function_name_are_two_nodes() {
    // Bash's function namespace is one flat table per *shell process*, not
    // per tree. A scan measures the tree: `usage` in two scripts is two
    // declarations, and merging them would make the definition census — this
    // track's whole deliverable — under-report by exactly the collisions.
    assert_ne!(
        function_fqn("bin/a.sh", &[], "usage"),
        function_fqn("bin/b.sh", &[], "usage"),
    );
    // And a script identity can never be spelled by a function one: the
    // script prefix is the same, so the `#` is what separates the two spaces
    // and a path the walk offers always carries an owned extension before it.
    assert_ne!(
        script_fqn("bin/a.sh"),
        function_fqn("bin/a.sh", &[], "usage")
    );
    assert!(script_fqn("bin/a.sh").starts_with('$'));
}

#[test]
fn an_enclosers_chain_spells_the_same_identity_as_the_definition() {
    // What `Encloser::as_definition` hands back for a nested function: a
    // plain definition whose owner is the chain. It must name the node the
    // definition phase filed.
    let facts = extract(
        "lib/bats-core/common.bash",
        "outer() {\n  inner() {\n    source lib/a.bash\n  }\n}\n",
    );
    let table: HashSet<NodeId> = HashSet::new();
    let from_def = facts
        .defs
        .iter()
        .find(|d| d.name == "inner")
        .expect("inner");
    let encloser = facts.refs[0]
        .enclosing
        .as_ref()
        .expect("an encloser")
        .as_definition()
        .expect("nameable");
    assert_eq!(
        BashResolver.def_fqn(&BashProject, &header(), &from_def.owner, from_def, &table),
        BashResolver.def_fqn(&BashProject, &header(), &encloser.owner, &encloser, &table),
    );
}

#[test]
fn a_function_redefined_in_one_file_is_one_slot() {
    // Legal bash: the second definition replaces the first and there is one
    // function at run time. A function and the script are never one entity.
    let a = def_of(DefKind::Function, "usage", &[], DefFacets::default());
    let b = def_of(DefKind::Function, "usage", &[], DefFacets::default());
    let c = def_of(DefKind::Function, "usage", &["outer"], DefFacets::default());
    assert!(BashResolver.mergeable(&a, &b));
    assert!(!BashResolver.mergeable(&a, &c));
}

#[test]
fn there_is_no_manifest_so_there_is_no_digest() {
    // Bash states nothing about its own layout outside its source, so a scan
    // of the same tree is never invalidated by a file the walk did not read.
    assert!(BashResolver.config_digest(&BashProject).is_empty());
    assert!(BashResolver.link_kinds().is_empty());
    assert_eq!(
        BashResolver.declared_container(&BashProject, &header()),
        None,
    );
}
