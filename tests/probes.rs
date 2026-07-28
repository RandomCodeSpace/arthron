//! The probe corpus: seven bug shapes, each asserted against the outcome its
//! own source documents.
//!
//! `corpus/go/probes` is the one corpus whose expected outcomes are written
//! down — every file states, in comments beside the code, what the resolver is
//! supposed to conclude. This file is the other half of that: it checks them.
//!
//! Why exact outcomes rather than a rate. The seven findings of the phase-2
//! adversarial review were all real bugs, two of them corrupting the
//! resolution rate directly, and fixing every one of them moved no count on
//! either Go corpus — neither codeiq nor caddy contains a triggering shape.
//! A rate cannot see a bug no corpus triggers. An assertion that names the
//! edge, the row and the node can, so each probe here asserts the *whole* row
//! set of the file it owns: a regression that adds an outcome is as visible as
//! one that changes an outcome.
//!
//! Three of the probes are not about a single scan at all — a build twin's
//! kind after one file is forgotten, a package clause rewritten under a stable
//! identity, a module directive that roots every FQN while owning no `.go`
//! bytes. Those are checked the way `tests/corpus.rs` checks the incremental
//! oracle: the event is applied to a copy, the warm store is compared with a
//! cold scan of the same tree, and the two must be equal.
//!
//! `baselines/go-probes.toml` is compared here too, but it is a **pin, not a
//! ratchet**. The probe corpus is hand-written, so its rate is not evidence of
//! a capability and must never be re-based to claim one: at 17 resolved, 1
//! local binding and nothing unresolved, any movement at all means a probe and
//! the resolver disagree, and the corpus's own provenance says to decide which
//! is wrong in `docs/decisions.md`.
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched clone would make a missing corpus look like a
//! broken resolver.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use arthron::extract_go::extract;
use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Lang, NodeId, RefKind, reason_name};
use arthron::pipeline::{scan_go, source_files};
use arthron::resolve_go::GoLang;
use arthron::store::{NodeRecord, Report, Snapshot, Store, StoredOutcome};

mod support;

const CORPUS: &str = "corpus/go/probes";
const BASELINE: &str = "baselines/go-probes.toml";
/// The module directive in `corpus/go/probes/go.mod`, which roots every FQN
/// below. Spelled once: probe 5 is that rewriting it renames the whole graph.
const MODULE: &str = "example.com/arthron/probes";

/// Whether the corpus has been cloned in.
fn corpus_present() -> bool {
    if Path::new(CORPUS).join("go.mod").is_file() {
        return true;
    }
    support::missing(Path::new(CORPUS));
    false
}

/// An FQN in the probe module.
fn q(suffix: &str) -> String {
    format!("{MODULE}{suffix}")
}

/// Scan the tree cold into a throwaway store and return everything it wrote.
fn scan_fresh(root: &Path) -> (Report, Snapshot) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let db = dir.path().join("graph.redb");
    let report = scan_go(root, &db).expect("the probe corpus scans");
    let store = Store::open(&db).expect("the store opens");
    let snapshot = store.snapshot().expect("the store snapshots");
    (report, snapshot)
}

/// One reference row, rendered so an expectation can be written the way the
/// probe's comment states it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Site {
    enclosing: String,
    raw_target: String,
    kind: u8,
    argc: Option<u32>,
    locally_bound: bool,
    count: u32,
    outcome: String,
}

/// What a resolved row names, by the node it names.
fn target_name(nodes: &BTreeMap<NodeId, NodeRecord>, id: &NodeId) -> String {
    match nodes.get(id) {
        Some(NodeRecord::Definition { fqn, .. }) => fqn.clone(),
        Some(NodeRecord::Package { import_path, .. }) => format!("package {import_path}"),
        Some(NodeRecord::External { package, .. }) => format!("external {package}"),
        None => panic!("a row resolves to {id:?}, which is no node in the store"),
    }
}

/// Every row of one file, sorted, so a whole file's outcome is one value.
fn sites_in(snapshot: &Snapshot, file: &str) -> Vec<Site> {
    let mut out: Vec<Site> = snapshot
        .rows
        .iter()
        .filter(|(key, _)| key.file == file)
        .map(|(key, record)| Site {
            enclosing: key.enclosing.clone(),
            raw_target: key.raw_target.clone(),
            kind: key.kind,
            argc: key.argc,
            locally_bound: key.locally_bound,
            count: record.count,
            outcome: match &record.outcome {
                StoredOutcome::Resolved(id) => {
                    format!("Resolved({})", target_name(&snapshot.nodes, id))
                }
                StoredOutcome::External(package) => format!("External({package})"),
                StoredOutcome::Unresolved(reason) => {
                    format!("Unresolved({})", reason_name(*reason))
                }
            },
        })
        .collect();
    out.sort();
    out
}

/// Assert a file's rows are exactly these, whatever order either side is
/// written in. The *whole* set, deliberately: a regression that adds an
/// outcome is as much a regression as one that changes an outcome, and a
/// per-row assertion cannot see the first kind.
fn expect_sites(snapshot: &Snapshot, file: &str, expected: Vec<Site>) {
    let mut expected = expected;
    expected.sort();
    assert_eq!(sites_in(snapshot, file), expected, "the rows of {file}");
}

/// A call site: the shape every probe below asserts.
fn call(
    enclosing: String,
    raw_target: &str,
    argc: u32,
    locally_bound: bool,
    outcome: &str,
) -> Site {
    Site {
        enclosing,
        raw_target: raw_target.to_string(),
        kind: RefKind::Call.code(),
        argc: Some(argc),
        locally_bound,
        count: 1,
        outcome: outcome.to_string(),
    }
}

/// A written type name that resolves to Go's universe scope.
///
/// Every probe below writes `int` and `string` in signatures and struct
/// fields, and each is a reference like any other — a predeclared name is
/// still a name, and the resolver says so with `External(go:builtin)` rather
/// than leaving it out of the count. `count` is a parameter because two `int`
/// results of one function share a row key, and the row records how many
/// sites it stands for.
fn builtin_type(enclosing: String, raw_target: &str, count: u32) -> Site {
    Site {
        enclosing,
        raw_target: raw_target.to_string(),
        kind: RefKind::TypeUse.code(),
        argc: None,
        locally_bound: false,
        count,
        outcome: "External(go:builtin)".to_string(),
    }
}

/// An import clause. Arity is `None`, not `Some(0)`: an import takes no
/// arguments, which is a different fact from taking zero.
fn import(enclosing: String, raw_target: &str, outcome: &str) -> Site {
    Site {
        enclosing,
        raw_target: raw_target.to_string(),
        kind: RefKind::Import.code(),
        argc: None,
        locally_bound: false,
        count: 1,
        outcome: outcome.to_string(),
    }
}

/// The node stored under an FQN, or a failure naming what was looked up.
fn node<'a>(snapshot: &'a Snapshot, fqn: &str) -> &'a NodeRecord {
    let id = arthron::model::node_id(Lang::Go.domain(), fqn);
    snapshot
        .nodes
        .get(&id)
        .unwrap_or_else(|| panic!("no node is stored for `{fqn}`"))
}

/// Assert a definition is stored under `fqn`, with this kind, declared at
/// exactly these `(file, line)` sites.
///
/// `matches!(node(…), NodeRecord::Definition { .. })` is what stood at four
/// of these call sites, and it is the weakest true thing that can be said
/// about a node: it passes for a definition of the wrong kind, under the
/// wrong name, sourced from the wrong file and the wrong line. A probe
/// exists to pin an *outcome*; this pins the whole record.
fn expect_definition(snapshot: &Snapshot, fqn: &str, kind: DefKind, sites: &[(&str, u32)]) {
    let NodeRecord::Definition {
        fqn: stored,
        kind: stored_kind,
        declarations,
        ..
    } = node(snapshot, fqn)
    else {
        panic!("{fqn} is not a definition");
    };
    assert_eq!(
        stored, fqn,
        "the node stored under {fqn} answers to another name"
    );
    assert_eq!(
        *stored_kind,
        kind.code(),
        "{fqn} is stored as {}, not {}",
        DefKind::from_code(*stored_kind).map_or("?", DefKind::name),
        kind.name(),
    );
    let got: Vec<(&str, u32)> = declarations
        .iter()
        .map(|d| (d.file.as_str(), d.line))
        .collect();
    assert_eq!(got, sites.to_vec(), "{fqn} declaration sites");
}

/// Assert a file owns no reference row at all.
///
/// Not a formality: `expect_sites` pins the rows of the files that carry
/// references, and the pin in `the_probe_pin_holds` pins the totals — but a
/// regression that invented a row in one of the remaining files and lost one
/// elsewhere would satisfy both. Saying "none" out loud is what closes that.
///
/// Fewer files qualify than once did. Every probe writes `int` or `string`
/// somewhere in a signature, and those are references now, so a file with no
/// call and no import is no longer a file with no row.
fn expect_no_sites(snapshot: &Snapshot, file: &str) {
    expect_sites(snapshot, file, vec![]);
}

/// Copy a tree so an event has something to edit. `corpus/` is pinned test
/// data and is never written to.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap_or_else(|e| panic!("creating {}: {e}", to.display()));
    for entry in fs::read_dir(from).unwrap_or_else(|e| panic!("reading {}: {e}", from.display())) {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target)
                .unwrap_or_else(|e| panic!("copying {}: {e}", entry.path().display()));
        }
    }
}

/// Scan `tree` cold into a throwaway store and compare it, whole, with what
/// the incremental scans left in `warm_db`.
fn assert_matches_cold(tree: &Path, warm_db: &Path, event: &str) {
    let cold_dir = tempfile::tempdir().expect("a scratch directory");
    let cold_db = cold_dir.path().join("cold.redb");
    let cold_report = scan_go(tree, &cold_db).expect("cold scan");
    let cold = Store::open(&cold_db)
        .expect("open cold")
        .snapshot()
        .expect("cold snapshot");

    let warm_store = Store::open(warm_db).expect("open warm");
    let warm = warm_store.snapshot().expect("warm snapshot");
    let warm_report = warm_store.report().expect("warm report");

    assert_eq!(
        cold, warm,
        "after {event}, the warm store is not what a cold scan builds"
    );
    assert_eq!(
        cold_report, warm_report,
        "after {event}, the reports differ"
    );
}

// -- probe 1: a clause header does not bind its own right-hand side ---------

#[test]
fn a_clause_header_resolves_its_right_hand_side_to_the_outer_binding() {
    // Go starts a declared identifier's scope at the END of its declaration,
    // so `if x := x()` names the outer `x` on the right. The bug this replaces
    // read the whole clause as bound and moved the right-hand side into
    // `LocalBinding`, which is outside both terms of the rate — it *raised*
    // the rate by deleting edges. Three headers, three edges, and no row in
    // the file may be locally bound.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh(Path::new(CORPUS));
    // The identity the three headers must reach, pinned before they are
    // asked to reach it: `Resolved(…#Producer)` says which node, and only
    // this says which declaration that node is.
    expect_definition(
        &snapshot,
        &q("/clauseheader#Producer"),
        DefKind::Function,
        &[("clauseheader/clauseheader.go", 14)],
    );
    let producer = format!("Resolved({})", q("/clauseheader#Producer"));
    expect_sites(
        &snapshot,
        "clauseheader/clauseheader.go",
        vec![
            // `BodyIsLocal` is the contrast case and owns no *call* row: a
            // bare identifier read is not a reference, so there is nothing
            // there for a scope bug to misfile — which is what makes the
            // three header rows below the whole of this file's call outcome.
            // Its result type is a written name like every other, and the
            // five type rows are what say the shadowing never reached one.
            builtin_type(q("/clauseheader#BodyIsLocal"), "int", 1),
            builtin_type(q("/clauseheader#ForHeader"), "int", 1),
            builtin_type(q("/clauseheader#IfHeader"), "int", 1),
            builtin_type(q("/clauseheader#Producer"), "int", 1),
            builtin_type(q("/clauseheader#SwitchHeader"), "string", 1),
            call(
                q("/clauseheader#ForHeader"),
                "Producer",
                0,
                false,
                &producer,
            ),
            call(q("/clauseheader#IfHeader"), "Producer", 0, false, &producer),
            call(
                q("/clauseheader#SwitchHeader"),
                "Producer",
                0,
                false,
                &producer,
            ),
        ],
    );
}

// -- probe 2: a row key carries the extractor's binding verdict -------------

#[test]
fn a_shadowed_call_and_its_package_level_twin_are_two_rows() {
    // Both sites agree on file, enclosing function, site text and arity, and
    // resolve differently. Without `locally_bound` in the key they are one
    // row, which keeps the first outcome and attributes both occurrences to
    // it: every count still sums, and the rate is wrong in both terms.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh(Path::new(CORPUS));
    // The package-level `Emit`, and the only one: the block-local `Emit` is
    // a func literal bound to a local and is not a node, so a second
    // declaration site here would be the shadowed binding leaking into the
    // graph — the other half of the bug this probe pins.
    expect_definition(
        &snapshot,
        &q("/shadowpair#Emit"),
        DefKind::Function,
        &[("shadowpair/shadowpair.go", 13)],
    );
    expect_sites(
        &snapshot,
        "shadowpair/shadowpair.go",
        vec![
            // `Emit`'s result, `Pair`'s two results, the `var inner string`
            // and the block-local literal's result: four sites, one row.
            builtin_type(q("/shadowpair#Emit"), "string", 1),
            builtin_type(q("/shadowpair#Pair"), "string", 4),
            call(
                q("/shadowpair#Pair"),
                "Emit",
                0,
                false,
                &format!("Resolved({})", q("/shadowpair#Emit")),
            ),
            call(
                q("/shadowpair#Pair"),
                "Emit",
                0,
                true,
                "Unresolved(LocalBinding)",
            ),
        ],
    );

    // And the call edge exists exactly once: the block-local call owes none.
    // Counted by kind, because `Pair` also owes a type-use edge to the
    // universe scope for the `string`s in its signature — a different kind of
    // edge, from a different reference, and not what this probe is about.
    let pair = arthron::model::node_id(Lang::Go.domain(), &q("/shadowpair#Pair"));
    let from_pair = snapshot
        .edges
        .iter()
        .filter(|(src, _, kind)| *src == pair && *kind == RefKind::Call.code())
        .count();
    assert_eq!(
        from_pair, 1,
        "Pair owes exactly one call edge, to the package-level Emit"
    );
}

// -- probe 3: a declaration site carries what *its* file declared ----------

#[test]
fn build_exclusive_twins_keep_both_declaration_sites() {
    // `//go:build linux` declares `Twin` as a function; `//go:build !linux`
    // declares the same FQN as a type. Both sites are stored, in (file, line)
    // order, and the record's kind is the first of them — a function of the
    // surviving set, not of write order.
    if !corpus_present() {
        return;
    }
    let (report, snapshot) = scan_fresh(Path::new(CORPUS));
    let NodeRecord::Definition {
        kind, declarations, ..
    } = node(&snapshot, &q("/buildtwin#Twin"))
    else {
        panic!("{} is not a definition", q("/buildtwin#Twin"));
    };
    let sites: Vec<(&str, u32, u8)> = declarations
        .iter()
        .map(|d| {
            let payload = match d.payload {
                arthron::store::NodePayload::Definition(k, _) => k,
                ref other => panic!("a twin site carries {other:?}"),
            };
            (d.file.as_str(), d.line, payload)
        })
        .collect();
    assert_eq!(
        sites,
        vec![
            ("buildtwin/twin_linux.go", 15, DefKind::Function.code()),
            ("buildtwin/twin_other.go", 10, DefKind::Type.code()),
        ],
        "both files' verdicts are kept, in (file, line) order",
    );
    assert_eq!(
        *kind,
        DefKind::Function.code(),
        "the record's kind is the first surviving site's, not the last writer's",
    );
    // Two files declaring one FQN is data, printed and never a gate.
    assert_eq!(report.fqn_collisions, 1);
    // Neither twin references anything, so neither owns a row. A twin that
    // grew one would be a build-constraint bug wearing a reference's clothes.
    // `func Twin() string` on one side, `type Twin struct{ Name string }` on
    // the other: two files, one written `string` each, and the enclosers
    // differ because a struct field sits at package level where a result type
    // sits inside the function.
    expect_sites(
        &snapshot,
        "buildtwin/twin_linux.go",
        vec![builtin_type(q("/buildtwin#Twin"), "string", 1)],
    );
    expect_sites(
        &snapshot,
        "buildtwin/twin_other.go",
        vec![builtin_type(q("/buildtwin"), "string", 1)],
    );
}

#[test]
fn forgetting_one_twin_leaves_the_surviving_files_kind() {
    // The half a single scan cannot show: keeping the last writer's answer
    // strands the departing file's kind on the survivor, and a warm store
    // disagrees with a cold one.
    if !corpus_present() {
        return;
    }
    let dir = tempfile::tempdir().expect("a scratch directory");
    let tree = dir.path().join("tree");
    copy_tree(Path::new(CORPUS), &tree);
    let warm_db = dir.path().join("warm.redb");
    scan_go(&tree, &warm_db).expect("first scan");

    fs::remove_file(tree.join("buildtwin/twin_linux.go")).expect("deleting the linux twin");
    scan_go(&tree, &warm_db).expect("rescan");
    assert_matches_cold(&tree, &warm_db, "the linux twin deleted");

    let store = Store::open(&warm_db).expect("open warm");
    let snapshot = store.snapshot().expect("warm snapshot");
    let NodeRecord::Definition {
        kind, declarations, ..
    } = node(&snapshot, &q("/buildtwin#Twin"))
    else {
        panic!("{} is not a definition", q("/buildtwin#Twin"));
    };
    assert_eq!(declarations.len(), 1, "one site survives");
    assert_eq!(
        *kind,
        DefKind::Type.code(),
        "the kind is re-derived from the site that remains",
    );
}

// -- probe 4: invalidation compares meaning, not only identity -------------

#[test]
fn two_variants_of_one_package_name_resolve_to_their_own_directories() {
    // Directory `beta` declares `package alpha`, beside a directory `alpha`
    // that declares `package alpha` too. The node is the import path, which
    // the directory decides; the declared name is what an unaliased import
    // binds, and is a fact in the package's source.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh(Path::new(CORPUS));
    for (fqn, expected) in [
        (q("/pkgrename/alpha"), "alpha"),
        (q("/pkgrename/beta"), "alpha"),
    ] {
        let NodeRecord::Package {
            import_path, name, ..
        } = node(&snapshot, &fqn)
        else {
            panic!("{fqn} is not a package");
        };
        assert_eq!(*import_path, fqn);
        assert_eq!(name.as_deref(), Some(expected));
    }
    // The two definitions the consumer's calls must land on, by kind and by
    // site: `Resolved(…/alpha#Name)` below is a statement about an identity,
    // and an identity sourced from the wrong file would still spell the same.
    expect_definition(
        &snapshot,
        &q("/pkgrename/alpha#Name"),
        DefKind::Function,
        &[("pkgrename/alpha/alpha.go", 6)],
    );
    expect_definition(
        &snapshot,
        &q("/pkgrename/beta#Name"),
        DefKind::Function,
        &[("pkgrename/beta/beta.go", 12)],
    );
    expect_sites(
        &snapshot,
        "pkgrename/alpha/alpha.go",
        vec![builtin_type(q("/pkgrename/alpha#Name"), "string", 1)],
    );
    expect_sites(
        &snapshot,
        "pkgrename/beta/beta.go",
        vec![builtin_type(q("/pkgrename/beta#Name"), "string", 1)],
    );
    expect_sites(
        &snapshot,
        "pkgrename/consumer.go",
        vec![
            builtin_type(q("/pkgrename#Both"), "string", 2),
            call(
                q("/pkgrename#Both"),
                "alpha.Name",
                0,
                false,
                &format!("Resolved({})", q("/pkgrename/alpha#Name")),
            ),
            call(
                q("/pkgrename#Both"),
                "beta.Name",
                0,
                false,
                &format!("Resolved({})", q("/pkgrename/beta#Name")),
            ),
            import(
                q("/pkgrename"),
                &q("/pkgrename/alpha"),
                &format!("Resolved(package {})", q("/pkgrename/alpha")),
            ),
            import(
                q("/pkgrename"),
                &q("/pkgrename/beta"),
                &format!("Resolved(package {})", q("/pkgrename/beta")),
            ),
        ],
    );
}

#[test]
fn rewriting_a_package_clause_moves_no_identity_and_still_changes_meaning() {
    // The invalidation probe. The package node is the import path, so this
    // edit moves no `NodeId` at all — and it changes what an unaliased import
    // of the directory binds, which is exactly the fact a store keyed on
    // identity alone would keep serving from before the edit.
    if !corpus_present() {
        return;
    }
    let dir = tempfile::tempdir().expect("a scratch directory");
    let tree = dir.path().join("tree");
    copy_tree(Path::new(CORPUS), &tree);
    let warm_db = dir.path().join("warm.redb");
    scan_go(&tree, &warm_db).expect("first scan");

    let clause = tree.join("pkgrename/beta/beta.go");
    let source = fs::read_to_string(&clause).expect("reading beta.go");
    let rewritten = source.replace("\npackage alpha\n", "\npackage gamma\n");
    assert_ne!(
        rewritten, source,
        "the package clause was not found to rewrite"
    );
    fs::write(&clause, &rewritten).expect("rewriting the package clause");
    scan_go(&tree, &warm_db).expect("rescan");
    assert_matches_cold(&tree, &warm_db, "beta's package clause rewritten");

    let store = Store::open(&warm_db).expect("open warm");
    let snapshot = store.snapshot().expect("warm snapshot");
    let NodeRecord::Package {
        import_path, name, ..
    } = node(&snapshot, &q("/pkgrename/beta"))
    else {
        panic!("the beta package node is gone, so the identity moved");
    };
    assert_eq!(
        *import_path,
        q("/pkgrename/beta"),
        "the identity is the path"
    );
    assert_eq!(
        name.as_deref(),
        Some("gamma"),
        "the declared name is re-read from the clause",
    );
}

// -- probe 5: the manifest is a scan input, so the store fences on it ------

#[test]
fn a_nested_module_is_excluded_and_its_definitions_never_appear() {
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh(Path::new(CORPUS));
    let nested: Vec<&String> = snapshot
        .files
        .keys()
        .filter(|f| f.starts_with("modfence/nested/"))
        .collect();
    assert!(
        nested.is_empty(),
        "a directory declaring its own go.mod belongs to another module: {nested:?}",
    );
    let names: Vec<&str> = snapshot
        .nodes
        .values()
        .filter_map(|n| match n {
            NodeRecord::Definition { fqn, .. } => Some(fqn.as_str()),
            _ => None,
        })
        .filter(|fqn| fqn.contains("NotOurs"))
        .collect();
    assert!(names.is_empty(), "the nested module contributed {names:?}");
    // And the package this module *does* own is rooted in the module
    // directive, which owns no `.go` bytes of its own.
    //
    // Pinned whole, not by `matches!(…, Definition { .. })`: the fact this
    // probe exists for is *which module roots the FQN*, and a record that
    // answered to the right name from the wrong file, at the wrong line, or
    // as the wrong kind would satisfy a match on the variant alone.
    expect_definition(
        &snapshot,
        &q("/modfence#Rooted"),
        DefKind::Function,
        &[("modfence/modfence.go", 14)],
    );
    // Neither file references anything, so the fence is visible in the rows
    // as well as in the nodes: the excluded file owns none because it was
    // never read, and the owned one owns none because it names nothing.
    expect_sites(
        &snapshot,
        "modfence/modfence.go",
        vec![builtin_type(q("/modfence#Rooted"), "string", 1)],
    );
    // The nested module is another module: this scan reads none of its
    // references, and the `string` in *its* signature is not this scan's to
    // count.
    expect_no_sites(&snapshot, "modfence/nested/nested.go");
}

#[test]
fn rewriting_the_module_directive_renames_every_fqn() {
    // `go.mod` has no extension the language owns and contributes no facts of
    // its own, yet its module directive roots every FQN in the corpus. A store
    // that fences on `.go` bytes alone computes an empty changed set here and
    // keeps a graph no cold scan would build.
    if !corpus_present() {
        return;
    }
    let dir = tempfile::tempdir().expect("a scratch directory");
    let tree = dir.path().join("tree");
    copy_tree(Path::new(CORPUS), &tree);
    let warm_db = dir.path().join("warm.redb");
    scan_go(&tree, &warm_db).expect("first scan");

    let manifest = tree.join("go.mod");
    let source = fs::read_to_string(&manifest).expect("reading go.mod");
    let renamed = "example.com/arthron/reprobed";
    let rewritten = source.replace(
        &format!("\nmodule {MODULE}\n"),
        &format!("\nmodule {renamed}\n"),
    );
    assert_ne!(rewritten, source, "the module directive was not found");
    fs::write(&manifest, &rewritten).expect("rewriting the module directive");
    scan_go(&tree, &warm_db).expect("rescan");
    assert_matches_cold(&tree, &warm_db, "the module directive rewritten");

    let store = Store::open(&warm_db).expect("open warm");
    let snapshot = store.snapshot().expect("warm snapshot");
    // The renamed module roots the FQN — and roots it at the same source
    // line, because no `.go` byte moved. A record that survived the rename
    // by being rebuilt from nothing would answer to the new name with no
    // declaration site at all.
    expect_definition(
        &snapshot,
        &format!("{renamed}/modfence#Rooted"),
        DefKind::Function,
        &[("modfence/modfence.go", 14)],
    );
    let stale = arthron::model::node_id(Lang::Go.domain(), &q("/modfence#Rooted"));
    assert!(
        !snapshot.nodes.contains_key(&stale),
        "the old FQN survived a scan no cold run would produce",
    );
}

#[test]
fn deleting_a_nested_manifest_hands_its_files_to_the_outer_module() {
    // No `.go` file's bytes move, and a directory that was excluded is now
    // scanned — which is why the set of nested module directories belongs in
    // the fenced digest and not only the file hashes.
    if !corpus_present() {
        return;
    }
    let dir = tempfile::tempdir().expect("a scratch directory");
    let tree = dir.path().join("tree");
    copy_tree(Path::new(CORPUS), &tree);
    let warm_db = dir.path().join("warm.redb");
    scan_go(&tree, &warm_db).expect("first scan");

    fs::remove_file(tree.join("modfence/nested/go.mod")).expect("deleting the nested manifest");
    scan_go(&tree, &warm_db).expect("rescan");
    assert_matches_cold(&tree, &warm_db, "the nested manifest deleted");

    let store = Store::open(&warm_db).expect("open warm");
    let snapshot = store.snapshot().expect("warm snapshot");
    assert!(
        snapshot.files.contains_key("modfence/nested/nested.go"),
        "the file the nested module owned is now the probe module's",
    );
    // And the definition it carries is rooted in the *outer* module now, at
    // the line it was always written on: the file's bytes never changed, so
    // anything else here is the fence and not the file.
    expect_definition(
        &snapshot,
        &q("/modfence/nested#NotOurs"),
        DefKind::Function,
        &[("modfence/nested/nested.go", 12)],
    );
}

// -- probe 6: `#` separates a container from its members -------------------

#[test]
fn a_dotted_directory_and_a_dotted_fqn_are_two_nodes() {
    // Under the old `{pkg}.{name}` grammar `func Foo` in package `p` and the
    // sibling directory `p.Foo` were one identity. A definition FQN now
    // carries exactly one `#` and a container FQN carries none.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh(Path::new(CORPUS));
    let NodeRecord::Definition { fqn, .. } = node(&snapshot, &q("/dotted/p#Foo")) else {
        panic!("the function Foo is not a definition");
    };
    assert_eq!(
        fqn.matches('#').count(),
        1,
        "a definition FQN carries one `#`"
    );
    let NodeRecord::Package {
        import_path, name, ..
    } = node(&snapshot, &q("/dotted/p.Foo"))
    else {
        panic!("the dotted directory is not a package");
    };
    assert_eq!(
        import_path.matches('#').count(),
        0,
        "a container FQN carries none"
    );
    assert_eq!(
        name.as_deref(),
        Some("pfoo"),
        "an unaliased import binds the declared name, which cannot contain a dot",
    );
    // Both halves of the old collision, pinned whole. Under the `{pkg}.{name}`
    // grammar these were one identity carrying both files' declaration sites,
    // so "one site each, from its own file" is the outcome that regressed.
    expect_definition(
        &snapshot,
        &q("/dotted/p#Foo"),
        DefKind::Function,
        &[("dotted/p/p.go", 13)],
    );
    expect_definition(
        &snapshot,
        &q("/dotted/p.Foo#Bar"),
        DefKind::Function,
        &[("dotted/p.Foo/pfoo.go", 11)],
    );
    expect_sites(
        &snapshot,
        "dotted/p/p.go",
        vec![builtin_type(q("/dotted/p#Foo"), "string", 1)],
    );
    expect_sites(
        &snapshot,
        "dotted/p.Foo/pfoo.go",
        vec![builtin_type(q("/dotted/p.Foo#Bar"), "string", 1)],
    );
    expect_sites(
        &snapshot,
        "dotted/consumer.go",
        vec![
            builtin_type(q("/dotted#Both"), "string", 2),
            call(
                q("/dotted#Both"),
                "p.Foo",
                0,
                false,
                &format!("Resolved({})", q("/dotted/p#Foo")),
            ),
            call(
                q("/dotted#Both"),
                "pfoo.Bar",
                0,
                false,
                &format!("Resolved({})", q("/dotted/p.Foo#Bar")),
            ),
            import(
                q("/dotted"),
                &q("/dotted/p"),
                &format!("Resolved(package {})", q("/dotted/p")),
            ),
            import(
                q("/dotted"),
                &q("/dotted/p.Foo"),
                &format!("Resolved(package {})", q("/dotted/p.Foo")),
            ),
        ],
    );
}

// -- probe 7: both phases decide container identity with one set of names --

#[test]
fn only_the_directory_decides_which_clause_is_an_external_test_package() {
    // `testpkg/api_test/` is a production package legitimately named
    // `api_test`; `testpkg/api/api_ext_test.go` carries the identical clause
    // meaning the opposite. The reserved `!` marks the external test
    // container, and an in-package test files in the production one.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh(Path::new(CORPUS));
    for (fqn, declared, sites) in [
        (q("/testpkg/api"), "api", vec!["testpkg/api/api.go"]),
        (
            q("/testpkg/api!test"),
            "api_test",
            vec!["testpkg/api/api_ext_test.go"],
        ),
        (
            q("/testpkg/api_test"),
            "api_test",
            vec!["testpkg/api_test/api.go", "testpkg/api_test/api_test.go"],
        ),
        (
            q("/testpkg/api_test!test"),
            "api_test_test",
            vec!["testpkg/api_test/api_ext_test.go"],
        ),
    ] {
        let NodeRecord::Package {
            import_path,
            name,
            declarations,
        } = node(&snapshot, &fqn)
        else {
            panic!("{fqn} is not a package");
        };
        assert_eq!(*import_path, fqn);
        assert_eq!(
            name.as_deref(),
            Some(declared),
            "{fqn} declares `{declared}`"
        );
        let files: Vec<&str> = declarations.iter().map(|d| d.file.as_str()).collect();
        assert_eq!(files, sites, "{fqn} is declared by exactly these files");
        assert_eq!(
            import_path.matches('!').count(),
            usize::from(fqn.ends_with("!test")),
            "`!` is reserved for the external test container",
        );
    }

    // The in-package test's own definition is filed in the production
    // container, and calls Serve there with no import.
    //
    // Whole, not `matches!(…, Definition { .. })`: this probe exists because
    // the bug filed `CallServe` under one namespace and sourced its edge at
    // another, and a match on the variant alone is true of both answers.
    expect_definition(
        &snapshot,
        &q("/testpkg/api_test#CallServe"),
        DefKind::Function,
        &[("testpkg/api_test/api_test.go", 18)],
    );
    // The three `Serve` declarations the containers keep apart. Two are
    // spelled identically in the source and differ only by the directory
    // they sit in, which is the whole of what this probe asserts.
    expect_definition(
        &snapshot,
        &q("/testpkg/api#Serve"),
        DefKind::Function,
        &[("testpkg/api/api.go", 6)],
    );
    expect_definition(
        &snapshot,
        &q("/testpkg/api_test#Serve"),
        DefKind::Function,
        &[("testpkg/api_test/api.go", 11)],
    );
    expect_definition(
        &snapshot,
        &q("/testpkg/api!test#ExerciseServe"),
        DefKind::Function,
        &[("testpkg/api/api_ext_test.go", 16)],
    );
    expect_definition(
        &snapshot,
        &q("/testpkg/api_test!test#ExerciseServe"),
        DefKind::Function,
        &[("testpkg/api_test/api_ext_test.go", 13)],
    );
    expect_sites(
        &snapshot,
        "testpkg/api/api.go",
        vec![builtin_type(q("/testpkg/api#Serve"), "string", 1)],
    );
    expect_sites(
        &snapshot,
        "testpkg/api_test/api.go",
        vec![builtin_type(q("/testpkg/api_test#Serve"), "string", 1)],
    );
    expect_sites(
        &snapshot,
        "testpkg/api_test/api_test.go",
        vec![
            builtin_type(q("/testpkg/api_test#CallServe"), "string", 1),
            call(
                q("/testpkg/api_test#CallServe"),
                "Serve",
                0,
                false,
                &format!("Resolved({})", q("/testpkg/api_test#Serve")),
            ),
        ],
    );
    expect_sites(
        &snapshot,
        "testpkg/api/api_ext_test.go",
        vec![
            builtin_type(q("/testpkg/api!test#ExerciseServe"), "string", 1),
            call(
                q("/testpkg/api!test#ExerciseServe"),
                "api.Serve",
                0,
                false,
                &format!("Resolved({})", q("/testpkg/api#Serve")),
            ),
            import(
                q("/testpkg/api!test"),
                &q("/testpkg/api"),
                &format!("Resolved(package {})", q("/testpkg/api")),
            ),
        ],
    );
    expect_sites(
        &snapshot,
        "testpkg/api_test/api_ext_test.go",
        vec![
            builtin_type(q("/testpkg/api_test!test#ExerciseServe"), "string", 1),
            call(
                q("/testpkg/api_test!test#ExerciseServe"),
                "api_test.Serve",
                0,
                false,
                &format!("Resolved({})", q("/testpkg/api_test#Serve")),
            ),
            import(
                q("/testpkg/api_test!test"),
                &q("/testpkg/api_test"),
                &format!("Resolved(package {})", q("/testpkg/api_test")),
            ),
        ],
    );
}

#[test]
fn an_in_package_test_files_the_same_way_warm_as_cold() {
    // The bug this probe exists for filed the in-package test under one
    // namespace and sourced its edges at another, because phase 1 saw only
    // what earlier scans had stored and phase 2 what phase 1 had just written.
    // Only a second scan over a warm store can tell them apart.
    if !corpus_present() {
        return;
    }
    let dir = tempfile::tempdir().expect("a scratch directory");
    let tree = dir.path().join("tree");
    copy_tree(Path::new(CORPUS), &tree);
    let warm_db = dir.path().join("warm.redb");
    scan_go(&tree, &warm_db).expect("first scan");

    let victim = tree.join("testpkg/api_test/api_test.go");
    let original = fs::read_to_string(&victim).expect("reading the in-package test");
    fs::write(&victim, format!("{original}\n// arthron probe\n")).expect("touching it");
    scan_go(&tree, &warm_db).expect("rescan");
    assert_matches_cold(&tree, &warm_db, "the in-package test touched");

    fs::remove_file(&victim).expect("deleting the in-package test");
    scan_go(&tree, &warm_db).expect("rescan");
    assert_matches_cold(&tree, &warm_db, "the in-package test deleted");

    fs::write(&victim, &original).expect("restoring the in-package test");
    scan_go(&tree, &warm_db).expect("rescan");
    assert_matches_cold(&tree, &warm_db, "the in-package test restored");
}

// -- the whole corpus ------------------------------------------------------

/// Whether a repo-relative file sits under a directory that declares its own
/// `go.mod` — a module this scan does not own.
fn in_a_nested_module(root: &Path, rel: &str) -> bool {
    let mut dir = match rel.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => return false, // a file at the module root
    };
    loop {
        if root.join(dir).join("go.mod").is_file() {
            return true;
        }
        match dir.rsplit_once('/') {
            Some((parent, _)) => dir = parent,
            None => return false,
        }
    }
}

#[test]
fn every_probe_reference_has_exactly_one_stored_outcome() {
    // The never-drop rule, on a corpus small enough to read: the four reported
    // columns partition the references the extractor emitted, exactly.
    if !corpus_present() {
        return;
    }
    let corpus = Path::new(CORPUS);
    let (report, _) = scan_fresh(corpus);
    let go = &report.per_lang[&Lang::Go.code()];

    let mut extracted = 0u64;
    for path in source_files::<GoLang>(corpus).expect("walking the corpus") {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        // The walk reaches every `.go` file under the root; the *scan* owns
        // only the files of this module. A directory with a `go.mod` of its
        // own is another module — `modfence/nested` is one, on purpose — and
        // its references belong to a scan nobody ran. Counting them here
        // would read a deliberate exclusion as a dropped reference.
        //
        // The exclusion is stated by path and not by "produced no rows",
        // which would be circular and would hide the drop this test exists to
        // catch. `a_nested_module_is_excluded_and_its_definitions_never_appear`
        // is what holds the exclusion itself honest.
        if in_a_nested_module(corpus, &rel) {
            continue;
        }
        let source =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        extracted += extract(&rel, &source).refs.len() as u64;
    }
    let stored = go.resolved + go.external + go.local_binding + go.unresolved_total();
    assert_eq!(
        stored,
        extracted,
        "resolved {} + external {} + local-binding {} + unresolved {} must equal the \
         {extracted} references the extractor found",
        go.resolved,
        go.external,
        go.local_binding,
        go.unresolved_total(),
    );
}

#[test]
fn the_probe_pin_holds() {
    // A pin, not a ratchet. Every outcome in this corpus is documented, so
    // there is no rate to improve: movement in any column means a probe and
    // the resolver disagree, and which one is wrong is a decision for
    // `docs/decisions.md`.
    if !corpus_present() {
        return;
    }
    let (report, _) = scan_fresh(Path::new(CORPUS));
    let go = &report.per_lang[&Lang::Go.code()];
    let measured = Counts {
        resolved: go.resolved,
        external: go.external,
        local_binding: go.local_binding,
        unresolved: go.unresolved_total(),
    };
    println!(
        "probes       resolved {} external {} local-binding {} unresolved {}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );

    let text = std::fs::read_to_string(BASELINE).expect("the pin is committed");
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(baseline.language, Lang::Go.name());
    assert_eq!(baseline.corpus, CORPUS);
    assert_eq!(
        baseline.counts, measured,
        "the probe corpus is pinned exactly, in every column",
    );
    match evaluate(&baseline, &measured) {
        GateVerdict::Pass { improved } => {
            assert!(!improved, "a probe corpus has no rate to improve")
        }
        other => panic!("{other:?}"),
    }
}
