//! The Python probe corpus: one method, four receivers, and every outcome
//! pinned by name.
//!
//! Why this file exists at all. A deep review found that method dispatch was
//! essentially unresolved across the engine and **no gate noticed**: 7
//! definitions, 176 call sites, zero resolved, green CI. `arthron gate`
//! compares four integers, and on django's 899 files one method call changing
//! bucket is noise. On six references it is a failed assertion naming the row.
//!
//! `corpus/python/probes` is four files and one call that matters:
//!
//! ```text
//! self.greet(name)         Resolved   probe.greeter#Greeter.greet
//! SHARED.greet("global")   Unresolved NeedsTypeInference
//! greeter.greet("annot")   Unresolved LocalBinding
//! receiver.greet("dyn")    Unresolved LocalBinding
//! ```
//!
//! The heritage clause is the only place this track reads a receiver's type
//! today, and `Loud(Greeter)` is a genuine cross-file hit: `Loud` is declared
//! in `caller.py`, `greet` in `greeter.py`.
//!
//! A probe is a truth table, not a showcase, so the three misses are pinned as
//! exactly as the hit. The annotated parameter is the one worth reading twice:
//! `greeter: Greeter` is the type on the page, it resolves as a **type use**
//! to `probe.greeter#Greeter`, and the call on the name it types resolves to
//! nothing at all. Two rows, one annotation, and only the type use is an edge.
//! The day an annotation types a receiver, that row moves and
//! `receiver.greet` — which has no annotation — must not.
//!
//! `baselines/python-probes.toml` is compared here too, and like the Go probe
//! pin it is a **pin, not a ratchet**: the corpus is hand-written, so its rate
//! is not evidence of a capability and must never be re-based to claim one.
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored).

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Lang, NodeId, RefKind, reason_name};
use arthron::store::{NodeRecord, Report, Snapshot, Store, StoredOutcome};
use arthron::track_python::resolve::scan_python;

mod support;

const CORPUS: &str = "corpus/python/probes";
const BASELINE: &str = "baselines/python-probes.toml";

const GREETER: &str = "probe/greeter.py";
const CALLER: &str = "probe/caller.py";
const INIT: &str = "probe/__init__.py";

/// The method every probe in this corpus is about.
const GREET: &str = "probe.greeter#Greeter.greet";
/// The class that declares it, one module away from every call.
const CLASS: &str = "probe.greeter#Greeter";

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
    let report = scan_python(Path::new(CORPUS), &db).expect("the probe corpus scans");
    let store = Store::open(&db).expect("the store opens");
    let snapshot = store.snapshot().expect("the store snapshots");
    (report, snapshot)
}

/// One reference row, rendered so an expectation reads the way the probe's
/// own docstring states it.
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

/// Assert a file owns no reference row at all. Saying "none" out loud is what
/// stops a regression that invents a row in one file and loses one in another
/// from satisfying both the per-file pins and the totals.
fn expect_no_sites(snapshot: &Snapshot, file: &str) {
    expect_sites(snapshot, file, vec![]);
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

/// A name read by an annotation. Python has no `new`, so a constructor call is
/// an ordinary [`RefKind::Call`]; the annotation beside it is this.
fn type_use(enclosing: &str, raw_target: &str, outcome: &str) -> Site {
    Site {
        enclosing: enclosing.to_string(),
        raw_target: raw_target.to_string(),
        kind: RefKind::TypeUse.code(),
        argc: None,
        locally_bound: false,
        count: 1,
        outcome: outcome.to_string(),
    }
}

/// A base named by a class's argument list.
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

/// An import clause. Arity is `None`, not `Some(0)`: an import takes no
/// arguments, which is a different fact from taking zero.
fn import(enclosing: &str, raw_target: &str, outcome: &str) -> Site {
    Site {
        enclosing: enclosing.to_string(),
        raw_target: raw_target.to_string(),
        kind: RefKind::Import.code(),
        argc: None,
        locally_bound: false,
        count: 1,
        outcome: outcome.to_string(),
    }
}

/// Assert a definition is stored under `fqn`, with this kind, declared at
/// exactly these `(file, line)` sites.
fn expect_definition(snapshot: &Snapshot, fqn: &str, kind: DefKind, sites: &[(&str, u32)]) {
    let id = arthron::model::node_id(Lang::Python.domain(), fqn);
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
fn a_heritage_clause_carries_self_across_the_module_boundary() {
    // The call this corpus exists for. `Loud(Greeter)` writes the receiver's
    // type down the one way this track reads it, `Greeter` is imported from
    // another module, and `self.greet` reaches the definition declared there.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    expect_definition(&snapshot, CLASS, DefKind::Type, &[(GREETER, 4)]);
    expect_definition(&snapshot, GREET, DefKind::Method, &[(GREETER, 5)]);
    expect_definition(
        &snapshot,
        "probe.caller#Loud",
        DefKind::Type,
        &[(CALLER, 26)],
    );
    expect_definition(
        &snapshot,
        "probe.caller#Loud.shout",
        DefKind::Method,
        &[(CALLER, 27)],
    );
    assert!(
        sites_in(&snapshot, CALLER).contains(&call(
            "probe.caller#Loud.shout",
            "self.greet",
            1,
            false,
            &format!("Resolved({GREET})"),
        )),
        "`self.greet` does not reach the base declared in greeter.py",
    );
}

#[test]
fn the_three_receivers_the_track_cannot_type_are_unresolved_with_their_reasons() {
    // The honest half of the truth table, and the half a showcase omits.
    //
    // `SHARED` is a module global built by a constructor call a few lines
    // above its use, and the track still does not type it —
    // `NeedsTypeInference`, inside the rate's denominator, which is where an
    // honest miss belongs.
    //
    // `greeter: Greeter` and the bare `receiver` land in the same bucket for
    // different reasons, and that is exactly why both are pinned: a local root
    // is outside *both* terms of the rate, so a regression that moved
    // `self.greet` there would raise the rate by deleting an edge.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    let rows = sites_in(&snapshot, CALLER);
    assert!(
        rows.contains(&call(
            "probe.caller#via_global",
            "SHARED.greet",
            1,
            false,
            "Unresolved(NeedsTypeInference)",
        )),
        "the module global's receiver is not reported as needing inference",
    );
    assert!(
        rows.contains(&call(
            "probe.caller#via_annotated",
            "greeter.greet",
            1,
            true,
            "Unresolved(LocalBinding)",
        )),
        "the annotated parameter is not where the locals policy puts it",
    );
    assert!(
        rows.contains(&type_use(
            "probe.caller#via_annotated",
            "Greeter",
            &format!("Resolved({CLASS})"),
        )),
        "the annotation itself must still resolve as a type use",
    );
    assert!(
        rows.contains(&call(
            "probe.caller#via_dynamic",
            "receiver.greet",
            1,
            true,
            "Unresolved(LocalBinding)",
        )),
        "the bare parameter is not where the locals policy puts it",
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
    let class = format!("Resolved({CLASS})");
    let greet = format!("Resolved({GREET})");

    expect_definition(
        &snapshot,
        "probe.caller#SHARED",
        DefKind::Var,
        &[(CALLER, 23)],
    );
    expect_definition(
        &snapshot,
        "probe.caller#Greeter",
        DefKind::Alias,
        &[(CALLER, 21)],
    );

    expect_sites(
        &snapshot,
        CALLER,
        vec![
            // The import, and the constructor call that builds the global.
            import("probe.caller", "probe.greeter.Greeter", &class),
            call("probe.caller", "Greeter", 0, false, &class),
            // The hit, and the clause that types its receiver.
            inherit("probe.caller#Loud", "Greeter", &class),
            call("probe.caller#Loud.shout", "self.greet", 1, false, &greet),
            // A global the track does not type.
            call(
                "probe.caller#via_global",
                "SHARED.greet",
                1,
                false,
                "Unresolved(NeedsTypeInference)",
            ),
            // An annotation that resolves, on a name whose call does not.
            type_use("probe.caller#via_annotated", "Greeter", &class),
            call(
                "probe.caller#via_annotated",
                "greeter.greet",
                1,
                true,
                "Unresolved(LocalBinding)",
            ),
            // And no written type at all.
            call(
                "probe.caller#via_dynamic",
                "receiver.greet",
                1,
                true,
                "Unresolved(LocalBinding)",
            ),
        ],
    );

    // The definition side names nothing, and the package marker is empty.
    expect_no_sites(&snapshot, GREETER);
    expect_no_sites(&snapshot, INIT);
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
    let python = &report.per_lang[&Lang::Python.code()];
    let measured = Counts {
        resolved: python.resolved,
        external: python.external,
        local_binding: python.local_binding,
        unresolved: python.unresolved_total(),
    };
    println!(
        "python probes resolved {} external {} local-binding {} unresolved {}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );

    // The reason tally, which no baseline records and no gate can see.
    support::assert_reasons(CORPUS, &python.unresolved, &[("NeedsTypeInference", 1)]);

    let text = std::fs::read_to_string(BASELINE).expect("the pin is committed");
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(baseline.language, Lang::Python.name());
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
