//! Resolver fixtures for Dart: one import model, one outcome per reference.
//!
//! The import model these pin is the one measured on the corpus: the URI's
//! *scheme* decides where the lookup happens, this repository's own package
//! name is tested before any dependency, and a URI that is not one literal
//! resolves against nothing and says so.

use std::collections::HashSet;

use arthron::UnresolvedReason::{self, *};
use arthron::lang::Resolver;
use arthron::model::{Domain, NodeId, node_id};
use arthron::track_dart::extract::extract;
use arthron::track_dart::lang::library_fqn;
use arthron::track_dart::project::{DartDep, DartProject};
use arthron::track_dart::resolve::DartResolver;
use arthron::{Outcome, resolution_rate};

/// A project with the given package name and declared dependencies, each of
/// them fetched from outside this repository.
fn project(package: Option<&str>, deps: &[&str]) -> DartProject {
    DartProject {
        package: package.map(str::to_string),
        dependencies: deps
            .iter()
            .map(|d| ((*d).to_string(), DartDep::External))
            .collect(),
        manifest: true,
    }
}

/// A project whose one dependency the manifest places *inside* this
/// repository, at the directory a `path:` entry names.
fn with_path_dep(package: &str, dep: &str, dir: &str) -> DartProject {
    DartProject {
        package: Some(package.to_string()),
        dependencies: [(dep.to_string(), DartDep::Local(dir.to_string()))]
            .into_iter()
            .collect(),
        manifest: true,
    }
}

/// A symbol table holding one library node per repo-relative `.dart` path.
fn table(files: &[&str]) -> HashSet<NodeId> {
    files
        .iter()
        .map(|f| node_id(Domain::Dart, &library_fqn(f)))
        .collect()
}

/// Every reference in one file, resolved.
fn outcomes(
    cfg: &DartProject,
    rel: &str,
    source: &str,
    known: &[&str],
) -> Vec<Outcome<NodeId, String>> {
    let known = table(known);
    let facts = extract(rel, source);
    let scope = DartResolver.scope(cfg, &facts, &known);
    facts
        .refs
        .iter()
        .map(|r| DartResolver.resolve(cfg, &scope, r, &known).outcome)
        .collect()
}

/// The single outcome of a one-reference file.
fn only(cfg: &DartProject, rel: &str, source: &str, known: &[&str]) -> Outcome<NodeId, String> {
    let mut got = outcomes(cfg, rel, source, known);
    assert_eq!(got.len(), 1, "{got:?}");
    got.remove(0)
}

fn reason(o: &Outcome<NodeId, String>) -> Option<&UnresolvedReason> {
    o.unresolved_reason()
}

fn resolved_to(o: &Outcome<NodeId, String>, file: &str) -> bool {
    *o == Outcome::Resolved(node_id(Domain::Dart, &library_fqn(file)))
}

// ---------------------------------------------------------------------------
// Relative URIs: against the referring library
// ---------------------------------------------------------------------------

#[test]
fn a_relative_uri_resolves_against_the_referring_files_directory() {
    let cfg = project(Some("collection"), &[]);
    let got = only(
        &cfg,
        "lib/src/wrappers.dart",
        "import 'unmodifiable_wrappers.dart';\n",
        &["lib/src/unmodifiable_wrappers.dart"],
    );
    assert!(
        resolved_to(&got, "lib/src/unmodifiable_wrappers.dart"),
        "{got:?}"
    );
}

#[test]
fn a_nested_directory_is_composed_and_not_flattened() {
    // `lib/src/combined_wrappers/` is a directory below the one every other
    // implementation file sits in. A resolver that joined against `lib/src`
    // would resolve this to a file that exists at the wrong depth, or miss a
    // file that is really there — a wrong edge either way.
    let cfg = project(Some("collection"), &[]);
    let got = only(
        &cfg,
        "lib/src/combined_wrappers/combined_iterable.dart",
        "import 'combined_iterator.dart';\n",
        &[
            "lib/src/combined_wrappers/combined_iterator.dart",
            "lib/src/combined_iterator.dart",
        ],
    );
    assert!(
        resolved_to(&got, "lib/src/combined_wrappers/combined_iterator.dart"),
        "{got:?}",
    );
}

#[test]
fn a_relative_uri_walks_up_out_of_its_own_directory() {
    let cfg = project(Some("collection"), &[]);
    let got = only(
        &cfg,
        "test/combined_wrapper/list_test.dart",
        "import '../unmodifiable_collection_test.dart' as common;\n",
        &["test/unmodifiable_collection_test.dart"],
    );
    assert!(
        resolved_to(&got, "test/unmodifiable_collection_test.dart"),
        "{got:?}"
    );
}

#[test]
fn a_relative_uri_naming_no_file_in_the_tree_is_module_not_found() {
    // The lookup is complete — every `.dart` file the walk reached is a
    // library node — so the literal really named no module here.
    let cfg = project(Some("collection"), &[]);
    let got = only(
        &cfg,
        "lib/src/wrappers.dart",
        "import 'gone.dart';\n",
        &["lib/src/wrappers.dart"],
    );
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
}

#[test]
fn a_relative_uri_climbing_past_the_repository_root_is_module_not_found() {
    let cfg = project(Some("collection"), &[]);
    let got = only(&cfg, "lib/a.dart", "import '../../elsewhere.dart';\n", &[]);
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
}

// ---------------------------------------------------------------------------
// package: URIs — this repository's own name first
// ---------------------------------------------------------------------------

#[test]
fn this_repositorys_own_package_uri_resolves_into_lib() {
    // The manifest is the only thing that connects the string `collection` in
    // the URI to the directory `lib/`; nothing in any `.dart` file does.
    let cfg = project(Some("collection"), &["test"]);
    let got = only(
        &cfg,
        "test/algorithms_test.dart",
        "import 'package:collection/collection.dart';\n",
        &["lib/collection.dart"],
    );
    assert!(resolved_to(&got, "lib/collection.dart"), "{got:?}");
}

#[test]
fn a_package_uri_reaches_a_files_own_subdirectory_under_lib() {
    let cfg = project(Some("collection"), &[]);
    let got = only(
        &cfg,
        "test/priority_queue_test.dart",
        "import 'package:collection/src/priority_queue.dart';\n",
        &["lib/src/priority_queue.dart"],
    );
    assert!(resolved_to(&got, "lib/src/priority_queue.dart"), "{got:?}");
}

#[test]
fn our_own_package_uri_naming_no_file_misses_rather_than_leaving_the_repository() {
    // The laundering this ordering exists to prevent. `External` sits outside
    // *both* terms of the resolution rate, so a self-referencing URI answered
    // `External` would take a real in-repository miss out of the measurement
    // entirely — the reference would vanish rather than fail.
    let cfg = project(Some("collection"), &["collection", "test"]);
    let got = only(
        &cfg,
        "test/a_test.dart",
        "import 'package:collection/gone.dart';\n",
        &["lib/collection.dart"],
    );
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
    assert!(
        !matches!(got, Outcome::External(_)),
        "an in-repository package URI was laundered as external: {got:?}",
    );
}

#[test]
fn a_package_uri_naming_our_own_package_and_no_path_names_no_library() {
    // Not a valid Dart URI, and the reason has to say which kind of nothing it
    // is: `UnknownPackage` would claim the name is outside this repository,
    // which is exactly what it is not.
    let cfg = project(Some("collection"), &["test"]);
    let got = only(&cfg, "lib/a.dart", "import 'package:collection';\n", &[]);
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
}

#[test]
fn a_declared_dependency_is_external_and_an_undeclared_one_is_not() {
    let cfg = project(Some("collection"), &["test"]);
    let declared = only(
        &cfg,
        "test/a_test.dart",
        "import 'package:test/test.dart';\n",
        &[],
    );
    assert_eq!(declared, Outcome::External("test".to_string()));
    // Nothing in the manifest says this name comes from outside, so it counts
    // *against* the rate rather than being waved through.
    let undeclared = only(
        &cfg,
        "test/a_test.dart",
        "import 'package:nowhere/x.dart';\n",
        &[],
    );
    assert_eq!(reason(&undeclared), Some(&UnknownPackage), "{undeclared:?}");
}

#[test]
fn a_path_dependency_is_a_lookup_under_its_own_package_and_not_an_external() {
    // The second half of the laundering defence. A `path:` entry states that
    // the package is a directory of this repository, so its `lib/` is one the
    // walk reached — and answering the URI `External` would take a reference
    // whose target *is* an in-repository node out of both terms of the rate.
    let cfg = with_path_dep("rootpkg", "other_pkg", "pkgs/other");
    let got = only(
        &cfg,
        "lib/main.dart",
        "import 'package:other_pkg/other.dart';\n",
        &["pkgs/other/lib/other.dart"],
    );
    assert!(resolved_to(&got, "pkgs/other/lib/other.dart"), "{got:?}");
    // A `./` prefix is the same directory: pub writes both.
    let dotted = with_path_dep("rootpkg", "other_pkg", "./pkgs/other");
    let got = only(
        &dotted,
        "lib/main.dart",
        "import 'package:other_pkg/src/deep.dart';\n",
        &["pkgs/other/lib/src/deep.dart"],
    );
    assert!(resolved_to(&got, "pkgs/other/lib/src/deep.dart"), "{got:?}");
}

#[test]
fn a_path_dependency_naming_no_file_misses_rather_than_leaving_the_repository() {
    // Same rule as this repository's own package: the lookup is complete, so
    // a literal that named none of the walked files really named no module
    // here — and the miss is counted, not laundered.
    let cfg = with_path_dep("rootpkg", "other_pkg", "pkgs/other");
    let got = only(
        &cfg,
        "lib/main.dart",
        "import 'package:other_pkg/gone.dart';\n",
        &["pkgs/other/lib/other.dart"],
    );
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
    assert!(
        !matches!(got, Outcome::External(_)),
        "an in-repository package URI was laundered as external: {got:?}",
    );
}

#[test]
fn a_path_dependency_pointing_above_the_repository_root_is_arthrons_own_gap() {
    // `path: ../sibling` from the root names a directory this scan never
    // walked and cannot see into. Nothing here proves no in-repository file
    // is behind the URI — a symlink is enough — so it is arthron's own gap,
    // counted against the rate rather than waved through as external.
    let cfg = with_path_dep("rootpkg", "sibling", "../sibling");
    let got = only(
        &cfg,
        "lib/main.dart",
        "import 'package:sibling/s.dart';\n",
        &[],
    );
    assert_eq!(reason(&got), Some(&ProjectLayoutUnknown), "{got:?}");
    assert!(!matches!(got, Outcome::External(_)), "{got:?}");
}

#[test]
fn without_a_manifest_a_package_uri_is_arthrons_own_gap() {
    // A different fact from a name the manifest was read and did not declare:
    // nothing in the tree says which package this repository is, so the miss
    // is this build's inference and the reason says so.
    let cfg = DartProject::default();
    let got = only(
        &cfg,
        "lib/a.dart",
        "import 'package:anything/x.dart';\n",
        &[],
    );
    assert_eq!(reason(&got), Some(&ProjectLayoutUnknown), "{got:?}");
    // A relative URI still resolves without a manifest: it needs no name.
    let relative = only(&cfg, "lib/a.dart", "import 'b.dart';\n", &["lib/b.dart"]);
    assert!(resolved_to(&relative, "lib/b.dart"), "{relative:?}");
}

// ---------------------------------------------------------------------------
// dart: URIs
// ---------------------------------------------------------------------------

#[test]
fn an_sdk_uri_is_external_under_its_whole_name() {
    // `dart:` is a scheme the language reserves for the SDK: no repository
    // file can be addressed by one, so calling it external cannot launder an
    // in-repository file out of the measurement.
    let cfg = project(Some("collection"), &[]);
    let got = only(
        &cfg,
        "lib/src/wrappers.dart",
        "import 'dart:collection';\n",
        &[],
    );
    assert_eq!(got, Outcome::External("dart:collection".to_string()));
}

#[test]
fn an_sdk_library_and_a_package_of_the_same_name_are_two_nodes() {
    // This corpus's own package is called `collection`, and so is an SDK
    // library. Naming the external node after the whole URI is what keeps
    // `dart:collection` from sharing an identity with the package.
    let cfg = project(Some("app"), &["collection"]);
    let sdk = only(&cfg, "lib/a.dart", "import 'dart:collection';\n", &[]);
    let pkg = only(
        &cfg,
        "lib/a.dart",
        "import 'package:collection/c.dart';\n",
        &[],
    );
    assert_eq!(sdk, Outcome::External("dart:collection".to_string()));
    assert_eq!(pkg, Outcome::External("collection".to_string()));
    assert_ne!(sdk, pkg);
}

#[test]
fn an_sdk_uri_is_external_from_an_export_as_much_as_from_an_import() {
    let cfg = project(Some("collection"), &[]);
    let got = only(
        &cfg,
        "lib/src/unmodifiable_wrappers.dart",
        "export 'dart:collection' show UnmodifiableListView;\n",
        &[],
    );
    assert_eq!(got, Outcome::External("dart:collection".to_string()));
}

// ---------------------------------------------------------------------------
// Exports, parts, and the shapes that resolve against nothing
// ---------------------------------------------------------------------------

#[test]
fn a_barrels_export_resolves_to_the_library_it_re_exports() {
    // The barrel's outgoing reference is the fact the source states. What the
    // re-export does to a *name* — which of its declarations reach an
    // importer through a `show` filter — is the export-map problem this tier
    // does not solve, and no alias is minted claiming it does.
    let cfg = project(Some("collection"), &[]);
    let got = outcomes(
        &cfg,
        "lib/collection.dart",
        "export 'src/algorithms.dart' show binarySearch, mergeSort;\nexport 'src/wrappers.dart';\n",
        &["lib/src/algorithms.dart", "lib/src/wrappers.dart"],
    );
    assert!(resolved_to(&got[0], "lib/src/algorithms.dart"), "{got:?}");
    assert!(resolved_to(&got[1], "lib/src/wrappers.dart"), "{got:?}");
}

#[test]
fn a_part_and_a_part_of_resolve_by_the_same_uri_rule() {
    let cfg = project(Some("collection"), &[]);
    let part = only(
        &cfg,
        "lib/a.dart",
        "part 'a_impl.dart';\n",
        &["lib/a_impl.dart"],
    );
    assert!(resolved_to(&part, "lib/a_impl.dart"), "{part:?}");
    let part_of = only(
        &cfg,
        "lib/a_impl.dart",
        "part of 'a.dart';\n",
        &["lib/a.dart"],
    );
    assert!(resolved_to(&part_of, "lib/a.dart"), "{part_of:?}");
}

#[test]
fn a_uri_that_is_not_one_literal_is_never_guessed() {
    let cfg = project(Some("collection"), &[]);
    let got = only(
        &cfg,
        "lib/a.dart",
        "import '${flavour}/b.dart';\n",
        &["lib/b.dart"],
    );
    assert_eq!(reason(&got), Some(&DynamicModuleSpecifier), "{got:?}");
}

#[test]
fn a_scheme_this_build_indexes_nothing_behind_is_unknown_package() {
    let cfg = project(Some("collection"), &[]);
    let got = only(&cfg, "lib/a.dart", "import 'file:///tmp/b.dart';\n", &[]);
    assert_eq!(reason(&got), Some(&UnknownPackage), "{got:?}");
}

#[test]
fn a_configurable_import_resolves_every_uri_it_names() {
    // Both are libraries this file may name; which one a reader compiles is a
    // configuration this scan cannot know. Choosing the default would drop
    // the other.
    let cfg = project(Some("collection"), &[]);
    let got = outcomes(
        &cfg,
        "lib/a.dart",
        "import 'stub.dart' if (dart.library.io) 'io.dart';\n",
        &["lib/stub.dart"],
    );
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(resolved_to(&got[0], "lib/stub.dart"), "{got:?}");
    assert_eq!(reason(&got[1]), Some(&ModuleNotFound), "{got:?}");
}

// ---------------------------------------------------------------------------
// The rate's own shape
// ---------------------------------------------------------------------------

#[test]
fn external_sits_outside_both_terms_and_a_miss_sits_inside_one() {
    let cfg = project(Some("collection"), &["test"]);
    let got = outcomes(
        &cfg,
        "test/a_test.dart",
        "import 'package:test/test.dart';\n\
         import 'dart:math';\n\
         import 'package:collection/collection.dart';\n\
         import 'gone.dart';\n",
        &["lib/collection.dart"],
    );
    let resolved = got.iter().filter(|o| o.is_resolved()).count() as u64;
    let external = got
        .iter()
        .filter(|o| matches!(o, Outcome::External(_)))
        .count() as u64;
    let unresolved = got.iter().filter_map(|o| o.unresolved_reason()).count() as u64;
    assert_eq!((resolved, external, unresolved), (1, 2, 1));
    // Two of the four references are in neither term: the rate is one in two,
    // not one in four.
    assert_eq!(resolution_rate(resolved, unresolved), Some(0.5));
}

#[test]
fn no_reference_this_track_emits_can_ever_be_a_local_binding() {
    // `LocalBinding` is the one bucket the rate's own definition lets a
    // resolver move references into without linking anything. Tier 2 emits no
    // expression-level reference, so nothing here can reach it.
    let cfg = project(Some("collection"), &["test"]);
    let got = outcomes(
        &cfg,
        "lib/a.dart",
        "import 'dart:math';\nimport 'b.dart';\nimport 'package:test/test.dart';\n\
         import 'gone.dart';\nimport '${x}.dart';\nexport 'c.dart';\n",
        &["lib/b.dart"],
    );
    assert!(
        got.iter().all(|o| reason(o) != Some(&LocalBinding)),
        "{got:?}",
    );
}
