//! Reviewer helper for comparing complete graph state across two scan stores.

use std::fmt::Debug;
use std::path::PathBuf;

use arthron::store::Store;

fn required_store(variable: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{variable} must name a store"))
}

fn same<T: Debug + PartialEq>(collection: &str, origin: &T, branch: &T) {
    assert_eq!(origin, branch, "first differing collection: {collection}",);
}

#[test]
#[ignore = "requires ARTHRON_COMPARE_ORIGIN_STORE and ARTHRON_COMPARE_BRANCH_STORE"]
fn two_stores_have_identical_graph_state() {
    let origin_path = required_store("ARTHRON_COMPARE_ORIGIN_STORE");
    let branch_path = required_store("ARTHRON_COMPARE_BRANCH_STORE");

    let origin = Store::open(&origin_path)
        .unwrap_or_else(|error| panic!("open {}: {error}", origin_path.display()))
        .snapshot()
        .unwrap_or_else(|error| panic!("snapshot {}: {error}", origin_path.display()));
    let branch = Store::open(&branch_path)
        .unwrap_or_else(|error| panic!("open {}: {error}", branch_path.display()))
        .snapshot()
        .unwrap_or_else(|error| panic!("snapshot {}: {error}", branch_path.display()));

    same("files", &origin.files, &branch.files);
    same("nodes", &origin.nodes, &branch.nodes);
    same(
        "collision_dispositions",
        &origin.collision_dispositions,
        &branch.collision_dispositions,
    );
    same("rows", &origin.rows, &branch.rows);
    same("edges", &origin.edges, &branch.edges);
    same("candidates", &origin.candidates, &branch.candidates);
    same("supers", &origin.supers, &branch.supers);
}
