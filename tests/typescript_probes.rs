//! The TypeScript probe corpus: one method, six receivers, and every outcome
//! pinned by name.
//!
//! Why this file exists at all. A deep review found that method dispatch was
//! essentially unresolved across the engine and **no gate noticed**: 7
//! definitions, 176 call sites, zero resolved, green CI. `arthron gate`
//! compares four integers, and on vue-core's 483 files one method call
//! changing bucket is noise. On fifteen references it is a failed assertion
//! naming the row.
//!
//! TypeScript carries the version of the question that matters most, because
//! it is the one tier-1 language where the receiver's type is *always* on the
//! page — and today the written type buys the call nothing:
//!
//! ```text
//! super.greet(name)          Resolved   greeter.ts#value:Greeter.prototype.greet
//! Greeter.make()             Resolved   greeter.ts#value:Greeter.make
//! this.greet(name)           Unresolved UnindexedSupertype
//! this.inner.greet('field')  Unresolved NoMatchingDefinition
//! shared.greet('module')     Unresolved NeedsTypeInference
//! greeter.greet('param')     Unresolved LocalBinding
//! ```
//!
//! Three annotations naming the class two lines of import away, each landing
//! in a *different* bucket without reaching the definition — while every one
//! of them resolves as a `TypeUse` to `greeter.ts#type:Greeter`. The type is
//! read; it is simply not used to type a receiver. A probe is a truth table,
//! so the misses are pinned as exactly as the hits: the day an annotation
//! types a receiver, three named rows move and this file says which.
//!
//! `super.greet` against `this.greet` is the same asymmetry
//! `tests/javascript_probes.rs` records, and it belongs to the shared
//! ECMAScript track rather than to either language — which is why both probes
//! assert it, in both dialects.
//!
//! `baselines/typescript-probes.toml` is compared here too, and like the Go
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

const CORPUS: &str = "corpus/typescript/probes";
const BASELINE: &str = "baselines/typescript-probes.toml";

const GREETER: &str = "greeter.ts";
const CALLER: &str = "caller.ts";

/// The instance method every probe in this corpus is about, the static beside
/// it, and the two nodes `Greeter` names — TypeScript declares a class in both
/// the value and the type space, and an annotation reads the second.
const GREET: &str = "greeter.ts#value:Greeter.prototype.greet";
const MAKE: &str = "greeter.ts#value:Greeter.make";
const VALUE: &str = "greeter.ts#value:Greeter";
const TYPE: &str = "greeter.ts#type:Greeter";

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

/// A written type annotation.
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
    let id = arthron::model::node_id(Lang::TypeScript.domain(), fqn);
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
    // binding, and `super.greet` reaches the method declared in greeter.ts.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    expect_definition(&snapshot, VALUE, DefKind::Type, &[(GREETER, 3)]);
    expect_definition(&snapshot, TYPE, DefKind::Type, &[(GREETER, 3)]);
    expect_definition(&snapshot, GREET, DefKind::Method, &[(GREETER, 4)]);
    expect_definition(
        &snapshot,
        "caller.ts#value:Loud.prototype.shout",
        DefKind::Method,
        &[(CALLER, 23)],
    );
    assert!(
        sites_in(&snapshot, CALLER).contains(&call(
            "caller.ts#value:Loud.prototype.shout",
            "super.greet",
            1,
            false,
            &format!("Resolved({GREET})"),
        )),
        "`super.greet` does not reach the base declared in greeter.ts",
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
            "caller.ts#value:viaStatic",
            "Greeter.make",
            0,
            false,
            &format!("Resolved({MAKE})"),
        )),
        "the static call does not reach {MAKE}",
    );
}

#[test]
fn every_written_annotation_resolves_and_types_no_receiver() {
    // The fact this corpus exists to state, and the one a rate cannot.
    //
    // `inner: Greeter`, `shared: Greeter` and `greeter: Greeter` all resolve —
    // as `TypeUse` rows, to the type-space node — and all three calls on the
    // names they type land in three *different* unresolved buckets. Pinning
    // the annotation beside the call is what makes the pair readable: the type
    // is not missing, it is not consulted.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    let rows = sites_in(&snapshot, CALLER);
    let annotated = format!("Resolved({TYPE})");
    for enclosing in [
        "caller.ts#value:Holder.prototype.inner",
        "caller.ts#value:shared",
        "caller.ts#value:viaParam",
    ] {
        assert!(
            rows.contains(&type_use(enclosing, "Greeter", &annotated)),
            "the annotation on {enclosing} does not resolve to {TYPE}",
        );
    }
    assert!(
        rows.contains(&call(
            "caller.ts#value:Holder.prototype.run",
            "this.inner.greet",
            1,
            false,
            "Unresolved(NoMatchingDefinition)",
        )),
        "the annotated field's call no longer reports the reason this probe pins",
    );
    assert!(
        rows.contains(&call(
            "caller.ts#value:viaModuleConst",
            "shared.greet",
            1,
            false,
            "Unresolved(NeedsTypeInference)",
        )),
        "the annotated module const's call no longer reports the reason this probe pins",
    );
    assert!(
        rows.contains(&call(
            "caller.ts#value:viaParam",
            "greeter.greet",
            1,
            true,
            "Unresolved(LocalBinding)",
        )),
        "the annotated parameter is not where the locals policy puts it",
    );
}

#[test]
fn this_does_not_make_the_walk_super_makes_and_says_so() {
    // The asymmetry, pinned as what is rather than what should be — the same
    // one `tests/javascript_probes.rs` records, because it belongs to the
    // shared track and not to either dialect.
    if !corpus_present() {
        return;
    }
    let (_, snapshot) = scan_fresh();
    expect_definition(
        &snapshot,
        "caller.ts#value:Loud.prototype.twice",
        DefKind::Method,
        &[(CALLER, 27)],
    );
    assert!(
        sites_in(&snapshot, CALLER).contains(&call(
            "caller.ts#value:Loud.prototype.twice",
            "this.greet",
            1,
            false,
            "Unresolved(UnindexedSupertype)",
        )),
        "`this.greet` no longer reports the reason this probe was written for",
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
    let value = format!("Resolved({VALUE})");
    let annotated = format!("Resolved({TYPE})");
    expect_definition(
        &snapshot,
        "caller.ts#value:Holder.prototype.inner",
        DefKind::Field,
        &[(CALLER, 33)],
    );
    expect_definition(
        &snapshot,
        "caller.ts#value:shared",
        DefKind::Const,
        &[(CALLER, 44)],
    );

    expect_sites(
        &snapshot,
        CALLER,
        vec![
            import("caller.ts", "./greeter.js", "Resolved(package greeter.ts)"),
            // The clause that types `super`, and the two calls it decides.
            inherit("caller.ts#value:Loud", "Greeter", &value),
            call(
                "caller.ts#value:Loud.prototype.shout",
                "super.greet",
                1,
                false,
                &format!("Resolved({GREET})"),
            ),
            call(
                "caller.ts#value:Loud.prototype.twice",
                "this.greet",
                1,
                false,
                "Unresolved(UnindexedSupertype)",
            ),
            // An annotated field: the annotation and the `new` both resolve,
            // the call through it does not.
            type_use(
                "caller.ts#value:Holder.prototype.inner",
                "Greeter",
                &annotated,
            ),
            new_site("caller.ts#value:Holder.prototype.inner", "Greeter", &value),
            call(
                "caller.ts#value:Holder.prototype.run",
                "this.inner.greet",
                1,
                false,
                "Unresolved(NoMatchingDefinition)",
            ),
            // The static, on the imported binding itself.
            type_use("caller.ts#value:viaStatic", "Greeter", &annotated),
            call(
                "caller.ts#value:viaStatic",
                "Greeter.make",
                0,
                false,
                &format!("Resolved({MAKE})"),
            ),
            // An annotated module const, and an annotated parameter.
            type_use("caller.ts#value:shared", "Greeter", &annotated),
            new_site("caller.ts#value:shared", "Greeter", &value),
            call(
                "caller.ts#value:viaModuleConst",
                "shared.greet",
                1,
                false,
                "Unresolved(NeedsTypeInference)",
            ),
            type_use("caller.ts#value:viaParam", "Greeter", &annotated),
            call(
                "caller.ts#value:viaParam",
                "greeter.greet",
                1,
                true,
                "Unresolved(LocalBinding)",
            ),
        ],
    );

    expect_sites(
        &snapshot,
        GREETER,
        vec![
            type_use("greeter.ts#value:Greeter.make", "Greeter", &annotated),
            new_site("greeter.ts#value:Greeter.make", "Greeter", &value),
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
    let ts = &report.per_lang[&Lang::TypeScript.code()];
    let measured = Counts {
        resolved: ts.resolved,
        external: ts.external,
        local_binding: ts.local_binding,
        unresolved: ts.unresolved_total(),
    };
    println!(
        "typescript probes resolved {} external {} local-binding {} unresolved {}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );

    // The reason tally, which no baseline records and no gate can see. Three
    // annotations, three different buckets — the breakdown is the payload
    // here, not a detail.
    support::assert_reasons(
        CORPUS,
        &ts.unresolved,
        &[
            ("NeedsTypeInference", 1),
            ("NoMatchingDefinition", 1),
            ("UnindexedSupertype", 1),
        ],
    );

    let text = std::fs::read_to_string(BASELINE).expect("the pin is committed");
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(baseline.language, Lang::TypeScript.name());
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
