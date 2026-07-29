//! The JavaScript probe corpus: one method, four receivers, and every outcome
//! pinned by name.
//!
//! Why this file exists at all. A deep review found that method dispatch was
//! essentially unresolved across the engine and **no gate noticed**: 7
//! definitions, 176 call sites, zero resolved, green CI. `arthron gate`
//! compares four integers, and on fastify's 266 files one method call changing
//! bucket is noise. On eight references it is a failed assertion naming the
//! row.
//!
//! `corpus/javascript/probes` is three files and one asymmetry that matters:
//!
//! ```text
//! super.greet(name)       Resolved   greeter.js#value:Greeter.prototype.greet
//! Greeter.make()          Resolved   greeter.js#value:Greeter.make
//! this.greet(name)        Unresolved UnindexedSupertype
//! shared.greet('module')  Unresolved NeedsTypeInference
//! receiver.greet('dyn')   Unresolved LocalBinding
//! ```
//!
//! The two hits are lookups rather than inference — a heritage clause written
//! in this file, and a member of an imported binding — which is what the
//! ECMAScript track deliberately buys instead of a type checker.
//!
//! `super.greet` against `this.greet` is the row worth reading twice: **the
//! same call, one keyword apart, with different answers.** `super.` walks the
//! written `extends` into the other module; `this.` does not make that walk
//! today.
//!
//! It also files the failure under the wrong reason, and that is pinned
//! deliberately rather than quietly. `src/lib.rs` defines
//! `UnresolvedReason::UnindexedSupertype` as "the receiver type is known and
//! in-repository, the member is in no indexed supertype, and at least one
//! supertype is external or unindexed". On this row **all three conjuncts are
//! false**: `Loud` is in-repository, the member *is* in an indexed supertype,
//! and `Greeter` is neither external nor unindexed — provably, because the
//! same scan resolves `super.greet` to
//! `greeter.js#value:Greeter.prototype.greet`. One missing branch causes it:
//! `walk_members` (`src/track_ecma/resolve.rs`) probes the base under a
//! module-local id, so an imported base always misses, and only
//! `resolve_super` carries the import-following fallback that recovers from
//! it.
//!
//! So the row is wrong twice over — a resolution this engine already holds
//! the facts to make, filed under a reason the taxonomy excludes. A probe is
//! a truth table, so both halves are asserted as what *is*, not as what
//! should be; recording the mislabel is what stops it being discovered later
//! as an unattributable movement.
//!
//! The fix's blast radius is measured, not guessed. Giving `resolve_this` the
//! fallback `resolve_super` already has moves exactly two baselines —
//! javascript/probes 75.0% to 87.5% (resolved 6 to 7, unresolved 2 to 1) and
//! typescript/probes 80.0% to 86.7% (resolved 12 to 13, unresolved 3 to 2) —
//! and moves **nothing** on express, fastify, vue-core or zod: all four hold
//! their four gated integers and their exact reason tallies. So it is a
//! deliberate re-base of two pins plus a `docs/decisions.md` entry for the
//! reason reported in the interval, and no ratchet is touched.
//!
//! `baselines/javascript-probes.toml` is compared here too, and like the Go
//! probe pin it is a **pin, not a ratchet**: the corpus is hand-written, so
//! its rate is not evidence of a capability and must never be re-based to
//! claim one.
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored).

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Lang, NodeId, RefKind, reason_name};
use arthron::store::{NodeRecord, Report, Snapshot, Store, StoredOutcome};
use arthron::track_ecma::scan_ecma;

mod support;

const CORPUS: &str = "corpus/javascript/probes";
const BASELINE: &str = "baselines/javascript-probes.toml";

const GREETER: &str = "greeter.js";
const CALLER: &str = "caller.js";

/// The instance method every probe in this corpus is about, and the static
/// beside it.
const GREET: &str = "greeter.js#value:Greeter.prototype.greet";
const MAKE: &str = "greeter.js#value:Greeter.make";
const CLASS: &str = "greeter.js#value:Greeter";

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
    let report = scan_ecma(Path::new(CORPUS), &db).expect("the probe corpus scans");
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
    let id = arthron::model::node_id(Lang::JavaScript.domain(), fqn);
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
fn a_written_heritage_clause_carries_super_across_the_module_boundary() {
    // The call this corpus exists for. `extends Greeter` names an imported
    // binding, and `super.greet` reaches the method declared in greeter.js.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    expect_definition(&snapshot, CLASS, DefKind::Type, &[(GREETER, 3)]);
    expect_definition(&snapshot, GREET, DefKind::Method, &[(GREETER, 4)]);
    expect_definition(
        &snapshot,
        "caller.js#value:Loud.prototype.shout",
        DefKind::Method,
        &[(CALLER, 20)],
    );
    assert!(
        sites_in(&snapshot, CALLER).contains(&call(
            "caller.js#value:Loud.prototype.shout",
            "super.greet",
            1,
            false,
            &format!("Resolved({GREET})"),
        )),
        "`super.greet` does not reach the base declared in greeter.js",
    );
}

#[test]
fn a_static_called_on_an_imported_binding_reaches_the_other_module() {
    // The second lookup that is not inference: the receiver *is* the imported
    // name, so the member is found on the definition it names.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    expect_definition(&snapshot, MAKE, DefKind::Method, &[(GREETER, 8)]);
    assert!(
        sites_in(&snapshot, CALLER).contains(&call(
            "caller.js#value:viaStatic",
            "Greeter.make",
            0,
            false,
            &format!("Resolved({MAKE})"),
        )),
        "the static call does not reach {MAKE}",
    );
}

#[test]
fn this_does_not_make_the_walk_super_makes_and_says_so() {
    // The asymmetry, pinned as what is rather than what should be.
    //
    // `Loud` extends the same imported `Greeter` in both cases and the scan
    // has `greeter.js` in hand — the row two tests above proves it. `this.`
    // still reports `UnindexedSupertype`, a reason whose definition in
    // `src/lib.rs` excludes this case on all three of its conjuncts; the
    // module doc reads it out in full. Both halves are recorded, the miss and
    // the mislabel, because that is what makes fixing them a movement of one
    // named row instead of a number nobody can attribute, and what stops the
    // row regressing further into a bucket outside the rate.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    expect_definition(
        &snapshot,
        "caller.js#value:Loud.prototype.twice",
        DefKind::Method,
        &[(CALLER, 24)],
    );
    assert!(
        sites_in(&snapshot, CALLER).contains(&call(
            "caller.js#value:Loud.prototype.twice",
            "this.greet",
            1,
            false,
            "Unresolved(UnindexedSupertype)",
        )),
        "`this.greet` no longer reports the reason this probe was written for",
    );
}

#[test]
fn the_two_receivers_the_track_cannot_type_are_unresolved_with_their_reasons() {
    // `shared` is a module const built by `new Greeter()` three lines above
    // its use and the track does not type it: `NeedsTypeInference`, inside the
    // rate's denominator, which is where an honest miss belongs. `receiver` is
    // a bare parameter, and a local root is outside *both* terms — pinned so
    // that a regression which moved a resolvable call there, raising the rate
    // by deleting an edge, has to move this row too.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    let rows = sites_in(&snapshot, CALLER);
    assert!(
        rows.contains(&call(
            "caller.js#value:viaModuleConst",
            "shared.greet",
            1,
            false,
            "Unresolved(NeedsTypeInference)",
        )),
        "the module const's receiver is not reported as needing inference",
    );
    assert!(
        rows.contains(&call(
            "caller.js#value:viaDynamic",
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
    expect_definition(
        &snapshot,
        "caller.js#value:shared",
        DefKind::Const,
        &[(CALLER, 33)],
    );

    expect_sites(
        &snapshot,
        CALLER,
        vec![
            import("caller.js", "./greeter.js", "Resolved(package greeter.js)"),
            // The clause that types `super`, and the two calls it decides.
            inherit("caller.js#value:Loud", "Greeter", &class),
            call(
                "caller.js#value:Loud.prototype.shout",
                "super.greet",
                1,
                false,
                &format!("Resolved({GREET})"),
            ),
            call(
                "caller.js#value:Loud.prototype.twice",
                "this.greet",
                1,
                false,
                "Unresolved(UnindexedSupertype)",
            ),
            // The static, on the imported binding itself.
            call(
                "caller.js#value:viaStatic",
                "Greeter.make",
                0,
                false,
                &format!("Resolved({MAKE})"),
            ),
            // A constructor call that resolves, feeding a receiver that does
            // not.
            new_site("caller.js#value:shared", "Greeter", &class),
            call(
                "caller.js#value:viaModuleConst",
                "shared.greet",
                1,
                false,
                "Unresolved(NeedsTypeInference)",
            ),
            call(
                "caller.js#value:viaDynamic",
                "receiver.greet",
                1,
                true,
                "Unresolved(LocalBinding)",
            ),
        ],
    );

    expect_sites(
        &snapshot,
        GREETER,
        vec![new_site("greeter.js#value:Greeter.make", "Greeter", &class)],
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
    let js = &report.per_lang[&Lang::JavaScript.code()];
    let measured = Counts {
        resolved: js.resolved,
        external: js.external,
        local_binding: js.local_binding,
        unresolved: js.unresolved_total(),
    };
    println!(
        "javascript probes resolved {} external {} local-binding {} unresolved {}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );

    // The reason tally, which no baseline records and no gate can see.
    support::assert_reasons(
        CORPUS,
        &js.unresolved,
        &[("NeedsTypeInference", 1), ("UnindexedSupertype", 1)],
    );

    let text = std::fs::read_to_string(BASELINE).expect("the pin is committed");
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(baseline.language, Lang::JavaScript.name());
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
