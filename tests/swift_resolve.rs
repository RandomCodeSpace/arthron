//! Resolver fixtures for Swift, scanned end to end: a package written to a
//! scratch tree, read the way `arthron scan` reads one, and every reference
//! accounted for.
//!
//! End to end rather than against a hand-built config, because the claim these
//! pin is about **phase 0 and the resolver together**: the resolver may call a
//! module "outside this package" only because the manifest enumerated the
//! modules inside it, and a manifest reader that quietly stopped seeing a
//! target would move every import of it into `External` — a bucket outside
//! both terms of the resolution rate, where the reference vanishes rather than
//! failing. Six of the tests below exist for exactly that failure, and two of
//! them are about the *partial* read rather than the total one: a manifest
//! that yields four targets out of five looks as read as one that yields all
//! five, and the module it did not yield is then spelled the way `Foundation`
//! is spelled.

use std::collections::BTreeMap;

use arthron::model::{Lang, reason_name};
use arthron::store::LangTally;
use arthron::track_swift::resolve::scan_swift;

/// A manifest declaring one library target and one test target.
const TWO_TARGETS: &str = "// swift-tools-version: 6.0\n\
    import PackageDescription\n\
    let package = Package(name: \"Demo\",\n\
        targets: [.target(name: \"Demo\", path: \"Source\"),\n\
                  .testTarget(name: \"DemoTests\", dependencies: [\"Demo\"], path: \"Tests\")])\n";

/// Write a tree and scan it, returning Swift's tally.
fn scan(files: &[(&str, &str)]) -> LangTally {
    let dir = tempfile::tempdir().expect("scratch tree");
    for (rel, source) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("tree");
        }
        std::fs::write(&path, source).expect("source");
    }
    let db = dir.path().join(".arthron").join("graph.redb");
    std::fs::create_dir_all(db.parent().expect("db dir")).expect("db dir");
    let report = scan_swift(dir.path(), &db).expect("the tree scans");
    report
        .per_lang
        .get(&Lang::Swift.code())
        .cloned()
        .unwrap_or_default()
}

/// A tally's unresolved reasons, by name.
fn reasons(tally: &LangTally) -> BTreeMap<&'static str, u64> {
    tally
        .unresolved
        .iter()
        .map(|(code, n)| (reason_name(*code), *n))
        .collect()
}

#[test]
fn a_test_target_importing_the_library_target_resolves_to_it() {
    // The one in-repository reference Swift's import surface offers: a test
    // module naming the module under test. The library's own 2 files import
    // nothing of each other and emit no reference at all — which is why the
    // resolved count here is 1 and not 3.
    let tally = scan(&[
        ("Package.swift", TWO_TARGETS),
        ("Source/A.swift", "import Foundation\nstruct A {}\n"),
        ("Source/B.swift", "struct B { let a = A() }\n"),
        (
            "Tests/ATests.swift",
            "@testable import Demo\nimport XCTest\nfinal class ATests: XCTestCase {}\n",
        ),
    ]);
    assert_eq!(tally.resolved, 1);
    // `Foundation`, `XCTest`, and the manifest's own `PackageDescription`.
    assert_eq!(tally.external, 3);
    assert_eq!(tally.unresolved_total(), 0);
    assert_eq!(tally.local_binding, 0);
}

#[test]
fn a_tree_with_no_manifest_launders_nothing_into_external() {
    // With no manifest there is no module namespace, so "outside this package"
    // is not a thing this build may assert about anything. Every import is
    // arthron's own inference failing, and it says so.
    let tally = scan(&[(
        "Source/A.swift",
        "import Foundation\nimport Demo\nstruct A {}\n",
    )]);
    assert_eq!(
        tally.external, 0,
        "an unread layout classified something as outside the package"
    );
    assert_eq!(tally.resolved, 0);
    assert_eq!(
        reasons(&tally),
        BTreeMap::from([("ProjectLayoutUnknown", 2)])
    );
}

#[test]
fn a_manifest_whose_target_list_cannot_be_read_launders_nothing_either() {
    // The manifest is there and parses; its `targets:` is a value this build
    // will not execute. An empty target list read as "the package builds no
    // module" would send every import in the tree to `External`.
    let tally = scan(&[
        (
            "Package.swift",
            "// swift-tools-version: 6.0\nimport PackageDescription\n\
             let package = Package(name: \"Demo\", targets: allTargets)\n",
        ),
        ("Source/A.swift", "import Foundation\nstruct A {}\n"),
    ]);
    assert_eq!(tally.external, 0);
    assert_eq!(
        reasons(&tally),
        BTreeMap::from([("ProjectLayoutUnknown", 2)])
    );
}

#[test]
fn a_target_list_read_only_in_part_launders_nothing_into_external() {
    // The hole the all-or-nothing guard left: this manifest states two
    // targets, and one of them names itself with a `let`. One target is read,
    // so "is the namespace known at all" answers yes — and `Gen` is a module
    // of this repository, with its sources in this tree, that would otherwise
    // be classified exactly the way `Foundation` is.
    let tally = scan(&[
        (
            "Package.swift",
            "// swift-tools-version: 6.0\nimport PackageDescription\n\
             let generated = \"Gen\"\n\
             let package = Package(name: \"Demo\",\n\
                 targets: [.target(name: \"Lib\", path: \"Sources/Lib\"),\n\
                           .target(name: generated, path: \"Sources/Gen\")])\n",
        ),
        (
            "Sources/Lib/A.swift",
            "import Gen\nimport Foundation\npublic struct A {}\n",
        ),
        ("Sources/Gen/B.swift", "public struct G {}\n"),
    ]);
    assert_eq!(
        tally.external, 0,
        "a namespace read in part classified something as outside the package",
    );
    assert_eq!(tally.resolved, 0);
    // `Gen`, `Foundation`, and the manifest's own `PackageDescription`: the
    // price of the guard is paid by the two that really are outside, in the
    // rate's denominator with a reason on them, rather than by the one that
    // is not, in a bucket outside both terms.
    assert_eq!(
        reasons(&tally),
        BTreeMap::from([("ProjectLayoutUnknown", 3)])
    );
}

#[test]
fn another_packages_manifest_in_the_tree_launders_nothing_either() {
    // Same hole, no computed name needed: a nested SwiftPM package states
    // targets in a manifest this reader does not read, built out of files
    // this walk *does* read. `NestedLib` is in this repository by any measure
    // that matters, and the root manifest has never heard of it.
    let tally = scan(&[
        (
            "Package.swift",
            "// swift-tools-version: 6.0\nimport PackageDescription\n\
             let package = Package(name: \"Demo\",\n\
                 targets: [.target(name: \"Lib\", path: \"Sources/Lib\"),\n\
                           .target(name: \"UsesNested\", path: \"Sources/UsesNested\")])\n",
        ),
        (
            "Nested/Package.swift",
            "// swift-tools-version: 6.0\nimport PackageDescription\n\
             let package = Package(name: \"Nested\",\n\
                 targets: [.target(name: \"NestedLib\", path: \"Sources/NestedLib\")])\n",
        ),
        ("Sources/Lib/A.swift", "public struct A {}\n"),
        (
            "Sources/UsesNested/U.swift",
            "import NestedLib\nimport Lib\npublic struct U {}\n",
        ),
        ("Nested/Sources/NestedLib/N.swift", "public struct N {}\n"),
    ]);
    assert_eq!(tally.external, 0);
    // A target the root manifest *does* state still resolves: the guard is
    // about names the enumeration does not contain, not about the ones it
    // does. Reading it as "give up on everything" would throw away the
    // in-repository links this track exists to count.
    assert_eq!(
        tally.resolved, 1,
        "import Lib is a target this package builds"
    );
    // `NestedLib`, and the two manifests' own `PackageDescription`.
    assert_eq!(
        reasons(&tally),
        BTreeMap::from([("ProjectLayoutUnknown", 3)])
    );
}

#[test]
fn an_import_of_a_declared_target_with_no_indexed_file_is_not_external() {
    // The shape that would hide a manifest-reader bug: a module the package
    // really does build, with nothing of it in the graph. `External` would put
    // it outside both rate terms and it would disappear; the honest answer
    // says the lookup was complete and the name is absent, which is a bug to
    // find rather than a reference to lose.
    let tally = scan(&[
        (
            "Package.swift",
            "// swift-tools-version: 6.0\nimport PackageDescription\n\
             let package = Package(name: \"Demo\",\n\
                 targets: [.target(name: \"Demo\", path: \"Source\"),\n\
                           .target(name: \"Empty\", path: \"Empty\")])\n",
        ),
        ("Source/A.swift", "import Empty\nstruct A {}\n"),
    ]);
    assert_eq!(tally.external, 1, "only PackageDescription is outside");
    assert_eq!(
        reasons(&tally),
        BTreeMap::from([("NoMatchingDefinition", 1)])
    );
}

#[test]
fn a_module_the_manifest_does_not_declare_is_external() {
    let tally = scan(&[
        ("Package.swift", TWO_TARGETS),
        (
            "Source/A.swift",
            "import Foundation\nimport Combine\nimport Security\nstruct A {}\n",
        ),
    ]);
    assert_eq!(tally.resolved, 0);
    // Three in `A.swift` plus the manifest's own `PackageDescription`.
    assert_eq!(tally.external, 4);
    assert_eq!(tally.unresolved_total(), 0);
}

#[test]
fn the_manifest_with_the_newest_tools_version_decides_the_module_names() {
    // SwiftPM picks among `Package@swift-*.swift` by toolchain version;
    // arthron runs no toolchain and reads the newest. Which one is read is a
    // fact about every identity in the graph, so it is pinned rather than left
    // to directory order.
    let old = "// swift-tools-version: 5.9\nimport PackageDescription\n\
        let package = Package(name: \"Demo\", targets: [.target(name: \"Old\", path: \"Source\")])\n";
    let new = "// swift-tools-version: 6.2\nimport PackageDescription\n\
        let package = Package(name: \"Demo\", targets: [.target(name: \"New\", path: \"Source\")])\n";
    let tally = scan(&[
        ("Package@swift-5.9.swift", old),
        ("Package@swift-6.2.swift", new),
        ("Source/A.swift", "struct A {}\n"),
        ("Tests/T.swift", "import New\nimport Old\n"),
    ]);
    // `New` is the module `Source/` builds under the manifest that was read;
    // `Old` is a name no target carries, so it is outside the package.
    assert_eq!(tally.resolved, 1);
    // `Old`, plus the two manifests' own `PackageDescription`.
    assert_eq!(tally.external, 3);
    assert_eq!(tally.unresolved_total(), 0);
}

#[test]
fn a_file_no_target_claims_is_its_own_module_and_still_contributes() {
    // A manifest is a `.swift` file the walk reads, and SwiftPM compiles it as
    // a module of its own. Its `import PackageDescription` is a reference like
    // any other, and its `let package` is a declaration like any other.
    let dir = tempfile::tempdir().expect("scratch tree");
    std::fs::create_dir_all(dir.path().join("Source")).expect("tree");
    std::fs::write(dir.path().join("Package.swift"), TWO_TARGETS).expect("manifest");
    std::fs::write(dir.path().join("Source/A.swift"), "struct A {}\n").expect("source");
    let db = dir.path().join("graph.redb");
    let report = scan_swift(dir.path(), &db).expect("scans");
    let tally = report
        .per_lang
        .get(&Lang::Swift.code())
        .cloned()
        .unwrap_or_default();
    assert_eq!(tally.external, 1);
    assert_eq!(tally.unresolved_total(), 0);

    let read = arthron::store::ReadStore::open(&db).expect("the store opens");
    let module = arthron::model::node_id(arthron::model::Domain::Swift, "$Package");
    let found = arthron::query::definition(&read, &module)
        .expect("query")
        .expect("the manifest is its own module");
    assert_eq!(found.node.kind, arthron::query::NodeKind::Package);
    let package_let = arthron::model::node_id(arthron::model::Domain::Swift, "$Package.package");
    assert!(
        arthron::query::definition(&read, &package_let)
            .expect("query")
            .is_some(),
        "the manifest's own declaration was lost",
    );
}

#[test]
fn an_excluded_directory_belongs_to_no_target() {
    // SwiftPM's `exclude:` takes files out of a target. A file that is in no
    // target is in no module, and its declarations must not land in one.
    let tally = scan(&[
        (
            "Package.swift",
            "// swift-tools-version: 6.0\nimport PackageDescription\n\
             let package = Package(name: \"Demo\",\n\
                 targets: [.target(name: \"Demo\", path: \"Source\", exclude: [\"Skipped\"])])\n",
        ),
        ("Source/A.swift", "struct A {}\n"),
        ("Source/Skipped/B.swift", "import Demo\nstruct B {}\n"),
    ]);
    // The excluded file is outside every target, so its `import Demo` is a
    // reference from a module of its own to the package's — and it resolves,
    // because `Demo` really is a module this package builds.
    assert_eq!(tally.resolved, 1);
    assert_eq!(tally.external, 1);
    assert_eq!(tally.unresolved_total(), 0);
}
