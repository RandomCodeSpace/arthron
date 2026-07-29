//! The Java probe corpus: one method, four receivers, and every outcome
//! pinned by name.
//!
//! Why this file exists at all. A deep review found that method dispatch was
//! essentially unresolved across the engine and **no gate noticed**: Go's
//! `.String()` had 7 definitions and 176 call sites, resolved zero, and CI was
//! green. Nothing in the gated numbers could see it. `arthron gate` compares
//! four integers, and on commons-lang's 527 files a method call that stops
//! resolving moves them by an amount nobody can attribute to a cause.
//!
//! So the commitment is a corpus small enough that a single call changing
//! bucket is a failed assertion naming the row. `corpus/java/probes` is three
//! files, fourteen references and one call that matters:
//!
//! ```text
//! greeter.greet("field")      Resolved   probe#Greeter.greet/1
//! this.greet(name)            Resolved   probe#Greeter.greet/1
//! local.greet("local")        Unresolved LocalBinding
//! make().greet("expression")  Unresolved NeedsExpressionType
//! ```
//!
//! A probe is a truth table, not a showcase. The two misses are pinned as
//! exactly as the two hits, because a fix has to be a deliberate re-base of a
//! named row rather than a number that drifted — and because
//! `local.greet` against `greeter.greet` is the same written type one keyword
//! apart, landing in different buckets by the one locals policy. A regression
//! that quietly moved the field call into `LocalBinding` would *raise* the
//! rate, which is the failure mode the policy exists to prevent.
//!
//! `baselines/java-probes.toml` is compared here too, and like the Go probe
//! pin it is a **pin, not a ratchet**: the corpus is hand-written, so its rate
//! is not evidence of a capability and must never be re-based to claim one.
//! Movement in any column means a probe and the resolver disagree, and which
//! one is wrong is a decision for `docs/decisions.md`.
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored).

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Lang, NodeId, RefKind, reason_name};
use arthron::store::{NodeRecord, Report, Snapshot, Store, StoredOutcome};
use arthron::track_java::scan_java;

mod support;

const CORPUS: &str = "corpus/java/probes";
const BASELINE: &str = "baselines/java-probes.toml";

const GREETER: &str = "src/main/java/probe/Greeter.java";
const CALLER: &str = "src/main/java/probe/Caller.java";
const LOUD: &str = "src/main/java/probe/Loud.java";

/// The method every probe in this corpus is about.
const GREET: &str = "probe#Greeter.greet/1";

/// Whether the corpus has been cloned in.
fn corpus_present() -> bool {
    if Path::new(CORPUS).join(GREETER).is_file() {
        return true;
    }
    support::missing(Path::new(CORPUS));
    false
}

/// Scan the tree cold into a throwaway store and return everything it wrote.
fn scan_fresh() -> (Report, Snapshot) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let db = dir.path().join("graph.redb");
    let report = scan_java(Path::new(CORPUS), &db).expect("the probe corpus scans");
    let store = Store::open(&db).expect("the store opens");
    let snapshot = store.snapshot().expect("the store snapshots");
    (report, snapshot)
}

/// One reference row, rendered so an expectation reads the way the probe's
/// own comment states it.
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

/// Assert a file's rows are exactly these. The *whole* set, deliberately: a
/// regression that adds an outcome is as much a regression as one that
/// changes an outcome, and a per-row assertion cannot see the first kind.
fn expect_sites(snapshot: &Snapshot, file: &str, expected: Vec<Site>) {
    let mut expected = expected;
    expected.sort();
    assert_eq!(sites_in(snapshot, file), expected, "the rows of {file}");
}

fn call(enclosing: &str, raw_target: &str, argc: u32, locally_bound: bool, outcome: &str) -> Site {
    Site {
        enclosing: enclosing.to_string(),
        raw_target: raw_target.to_string(),
        kind: RefKind::Call.code(),
        argc: Some(argc),
        locally_bound,
        count: 1,
        outcome: outcome.to_string(),
    }
}

/// A written type name in a signature or a declaration. `count` is a
/// parameter because two `String`s in one method share a row key, and the row
/// records how many sites it stands for.
fn type_use(enclosing: &str, raw_target: &str, count: u32, outcome: &str) -> Site {
    Site {
        enclosing: enclosing.to_string(),
        raw_target: raw_target.to_string(),
        kind: RefKind::TypeUse.code(),
        argc: None,
        locally_bound: false,
        count,
        outcome: outcome.to_string(),
    }
}

/// An object creation site. Arity is `Some(0)`, not `None`: `new Greeter()`
/// takes zero arguments, which is a different fact from taking none.
fn new_site(enclosing: &str, raw_target: &str, outcome: &str) -> Site {
    Site {
        enclosing: enclosing.to_string(),
        raw_target: raw_target.to_string(),
        kind: RefKind::New.code(),
        argc: Some(0),
        locally_bound: false,
        count: 1,
        outcome: outcome.to_string(),
    }
}

/// A supertype named by an `extends` clause.
fn inherit(enclosing: &str, raw_target: &str, outcome: &str) -> Site {
    Site {
        enclosing: enclosing.to_string(),
        raw_target: raw_target.to_string(),
        kind: RefKind::Inherit.code(),
        argc: None,
        locally_bound: false,
        count: 1,
        outcome: outcome.to_string(),
    }
}

/// Assert a definition is stored under `fqn`, with this kind, declared at
/// exactly these `(file, line)` sites.
///
/// `Resolved(…#Greeter.greet/1)` says which node a call reaches; only this
/// says which declaration that node is.
fn expect_definition(snapshot: &Snapshot, fqn: &str, kind: DefKind, sites: &[(&str, u32)]) {
    let id = arthron::model::node_id(Lang::Java.domain(), fqn);
    let Some(NodeRecord::Definition {
        fqn: stored,
        kind: stored_kind,
        declarations,
        ..
    }) = snapshot.nodes.get(&id)
    else {
        panic!("no definition is stored for `{fqn}`");
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

#[test]
fn a_field_whose_type_is_declared_reaches_the_method_declared_in_another_file() {
    // The call this whole corpus exists for. `greeter` is a field, its type is
    // written on the declaration in Caller.java, and `greet` is declared in
    // Greeter.java — one file away, which is the part a within-file resolver
    // gets wrong and reports as a pass.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    expect_definition(&snapshot, "probe#Greeter", DefKind::Type, &[(GREETER, 9)]);
    expect_definition(&snapshot, GREET, DefKind::Method, &[(GREETER, 10)]);
    expect_definition(
        &snapshot,
        "probe#Caller.greeter",
        DefKind::Field,
        &[(CALLER, 21)],
    );
    assert!(
        sites_in(&snapshot, CALLER).contains(&call(
            "probe#Caller.viaField/0",
            "greeter.greet",
            1,
            false,
            &format!("Resolved({GREET})"),
        )),
        "the field call does not reach {GREET}",
    );
}

#[test]
fn a_heritage_clause_carries_this_across_the_file_boundary() {
    // The second way a receiver's type is written down: `extends Greeter`
    // types `this`, and the method it inherits is declared in another file.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    expect_definition(&snapshot, "probe#Loud", DefKind::Type, &[(LOUD, 12)]);
    expect_definition(
        &snapshot,
        "probe#Loud.shout/1",
        DefKind::Method,
        &[(LOUD, 13)],
    );
    assert!(
        sites_in(&snapshot, LOUD).contains(&call(
            "probe#Loud.shout/1",
            "this.greet",
            1,
            false,
            &format!("Resolved({GREET})"),
        )),
        "`this.greet` does not reach the base declared in Greeter.java",
    );
}

#[test]
fn the_two_receivers_the_track_cannot_type_are_unresolved_with_their_reasons() {
    // The honest half of the truth table, and the half a showcase omits.
    //
    // `local.greet` writes the identical type `greeter.greet` does, on a local
    // instead of a field, and a local root is outside *both* terms of the
    // resolution rate under the one locals policy. It is pinned here so that a
    // regression which moved the field call into that bucket — raising the
    // rate by deleting an edge — cannot do it quietly.
    //
    // `make().greet` has no written type at all: the receiver is a call's
    // result, and naming it takes the type checker this engine deliberately
    // does not run. `NeedsExpressionType` is the reason for exactly that, and
    // the gate cannot see a relabelling, so it is pinned by name.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    let rows = sites_in(&snapshot, CALLER);
    assert!(
        rows.contains(&call(
            "probe#Caller.viaLocal/0",
            "local.greet",
            1,
            true,
            "Unresolved(LocalBinding)",
        )),
        "the local receiver is not where the locals policy puts it",
    );
    assert!(
        rows.contains(&call(
            "probe#Caller.viaExpression/0",
            "make().greet",
            1,
            false,
            "Unresolved(NeedsExpressionType)",
        )),
        "the expression receiver is not reported as needing an expression type",
    );
}

#[test]
fn every_row_of_the_probe_corpus_is_pinned() {
    // Not counts: names. Every row of every file, whole — because a
    // regression that invents a row is as visible here as one that changes an
    // outcome, and the four gated integers can see neither.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    let greeter = "Resolved(probe#Greeter)";
    let ctor = "Resolved(probe#Greeter.<init>/0)";
    let resolved_greet = format!("Resolved({GREET})");
    let jdk = "External(jdk:java.lang)";

    expect_sites(
        &snapshot,
        GREETER,
        vec![type_use("probe#Greeter.greet/1", "String", 2, jdk)],
    );

    expect_sites(
        &snapshot,
        CALLER,
        vec![
            // The field: its declared type, and the constructor that fills it.
            type_use("probe#Caller", "Greeter", 2, greeter),
            new_site("probe#Caller", "Greeter", ctor),
            // `make()`: return type and the type of the `new` in its body.
            type_use("probe#Caller.make/0", "Greeter", 2, greeter),
            new_site("probe#Caller.make/0", "Greeter", ctor),
            // The hit.
            type_use("probe#Caller.viaField/0", "String", 1, jdk),
            call(
                "probe#Caller.viaField/0",
                "greeter.greet",
                1,
                false,
                &resolved_greet,
            ),
            // The same written type on a local, in the bucket outside both
            // terms of the rate.
            type_use("probe#Caller.viaLocal/0", "Greeter", 2, greeter),
            new_site("probe#Caller.viaLocal/0", "Greeter", ctor),
            type_use("probe#Caller.viaLocal/0", "String", 1, jdk),
            call(
                "probe#Caller.viaLocal/0",
                "local.greet",
                1,
                true,
                "Unresolved(LocalBinding)",
            ),
            // No written type at all. `make` itself resolves; what it returns
            // does not.
            type_use("probe#Caller.viaExpression/0", "String", 1, jdk),
            call(
                "probe#Caller.viaExpression/0",
                "make",
                0,
                false,
                "Resolved(probe#Caller.make/0)",
            ),
            call(
                "probe#Caller.viaExpression/0",
                "make().greet",
                1,
                false,
                "Unresolved(NeedsExpressionType)",
            ),
        ],
    );

    expect_sites(
        &snapshot,
        LOUD,
        vec![
            inherit("probe#Loud", "Greeter", greeter),
            type_use("probe#Loud.shout/1", "String", 2, jdk),
            call(
                "probe#Loud.shout/1",
                "this.greet",
                1,
                false,
                &resolved_greet,
            ),
        ],
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
    let (report, _) = scan_fresh();
    let java = &report.per_lang[&Lang::Java.code()];
    let measured = Counts {
        resolved: java.resolved,
        external: java.external,
        local_binding: java.local_binding,
        unresolved: java.unresolved_total(),
    };
    println!(
        "java probes  resolved {} external {} local-binding {} unresolved {}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );

    // The reason tally, which no baseline records and no gate can see.
    support::assert_reasons(CORPUS, &java.unresolved, &[("NeedsExpressionType", 1)]);

    let text = std::fs::read_to_string(BASELINE).expect("the pin is committed");
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(baseline.language, Lang::Java.name());
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
