//! Acceptance for the Scala track against the upickle corpus: nothing is
//! dropped, the tier-2 contract holds on real code, and the measured counts
//! are the ones the committed baseline was recorded from.
//!
//! Scala is a **tier-2** language, so what this file gates is an
//! **import-resolution rate** — `Resolved / (Resolved + Unresolved)` over the
//! import references the extractor emits, one per *selector*, and nothing
//! else. It is not comparable with Go's or Java's rate, and it is never
//! aggregated with either.
//!
//! Four questions, because a rate is only worth reading if you can answer all
//! of them:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** Tier 2's deliverable is definitions, structure and
//!    imports, and the rate can only see the imports. The definition census
//!    is therefore asserted exactly on both sides of the store, by kind and
//!    by name — an owner-frame bug that lost most of the corpus's methods
//!    moves no rate, no bucket and no baseline, so nothing else here would
//!    notice it.
//! 3. **The union over build configurations.** upickle is built across five
//!    Scala versions and three platforms, and 15 source-root names across 47
//!    directories are selected among per build. arthron measures the *tree*,
//!    so several files declare one FQN — and the count of those is asserted,
//!    because a resolver that quietly merged them would report a cleaner
//!    graph than the repository has.
//! 4. **The ratchet.** The counts are compared against
//!    `baselines/scala-upickle.toml` through the same
//!    [`arthron::gate::evaluate`] the `arthron gate` command uses, so a rate
//!    regression — or drift in either of the two buckets that sit outside the
//!    rate — fails the build.
//!
//! upickle is pinned and is never edited, so every number below is a fact
//! about this extractor and this resolver reading a fixed 145 files; a change
//! to any of them is a change in what the track *does*, and must arrive as a
//! deliberate edit here and a deliberate `--rebase` beside it, never as a
//! test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/scala/upickle --language scala \
//!     --baseline baselines/scala-upickle.toml --rebase --commit 87e0b24
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken track.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Domain, Lang, RefKind, node_id, reason_name};
use arthron::pipeline::source_files;
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_scala::extract::extract;
use arthron::track_scala::lang::ScalaLang;
use arthron::track_scala::resolve::scan_scala;

const CORPUS: &str = "corpus/scala/upickle";
const BASELINE: &str = "baselines/scala-upickle.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
const FILES: usize = 145;
const REFERENCES: u64 = 631;

/// Import selectors by the form they were written in.
///
/// More than the 493 `import` *lines* the corpus provenance counts, and
/// deliberately so: this track emits one reference per **selector**, so
/// `import upickle.legacy.{ReadWriter => RW, Reader => R, Writer => W}` is
/// one line and three references, and `import scala.quoted.{ given, _ }` is
/// one line, one given-wildcard and one wildcard.
const FORMS: &[(&str, u64)] = &[
    ("given-wildcard", 17),
    ("named", 378),
    ("renamed", 29),
    ("wildcard", 207),
];

/// Every definition the extractor emits over those 145 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. Definitions
/// are the half of tier 2 the import-rate gate cannot see: an owner-frame bug
/// that lost most of the methods in the corpus would leave every rate, every
/// bucket and the whole ratchet untouched.
///
/// `Module` counts three different things Scala spells three ways and this
/// track files in one namespace: a package (one definition per prefix, per
/// file that writes the clause), a `package object`, and an `object`.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 552),
    (DefKind::Method, 700),
    (DefKind::Type, 455),
    (DefKind::Const, 283),
    (DefKind::Var, 40),
    (DefKind::Constructor, 14),
    (DefKind::Module, 592),
];

/// Definition nodes the store holds, by kind.
///
/// Lower than [`DEFS`] where one FQN is written more than once — an overload
/// pair in one class, or a name two source roots each declare. The pair of
/// censuses is the point: the extractor's says nothing was lost on the way
/// in, the store's says nothing was lost or over-merged on the way through.
///
/// `DefKind::Module` is absent because the driver files a module as a
/// *package* node rather than a definition; those are counted by
/// [`PACKAGES`] instead.
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Function, 534),
    (DefKind::Method, 686),
    (DefKind::Type, 438),
    (DefKind::Const, 283),
    (DefKind::Var, 40),
    (DefKind::Constructor, 14),
];

/// Package nodes: every package this repository declares, every `object`, and
/// the unnamed root package two files sit in — 592 module definitions in, 375
/// identities out, which is what `package upickle.core` written in
/// twenty-seven files looks like from the other side.
const PACKAGES: u64 = 375;

/// External nodes. **Zero, and asserted rather than observed.** Scala's
/// platform roots and its build's Maven coordinates are both outside this
/// repository and neither can be named here without guessing, so every path
/// that leaves the repository counts against the rate instead of leaving its
/// denominator. See `track_scala::resolve` for the argument.
const EXTERNALS: u64 = 0;

/// Distinct FQNs a definition in more than one file claims.
///
/// The union over build configurations, counted. `upickle.WebJson` is written
/// in `src-js`, `src-jvm` and `src-native`; `upickle.core.compat.Factory` in
/// `src-2.12` and `src-2.13+`. Every one is real under its own build and none
/// is merged, because merging would report a cleaner graph than the
/// repository has.
const COLLISIONS: u64 = 17;

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. A companion `trait
/// Visitor` and `object Visitor` cannot both be right unless the term and
/// type namespaces stayed apart, and `ujson.read` cannot be right unless a
/// `package object`'s members landed in the package.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // The companion pair, thirteen lines apart in one file: the trait is a
    // member of the package and the object *is* a container, so they are two
    // identities and a flat dotted name would have merged them.
    (
        "_root_.upickle.core#Visitor",
        NodeKind::Definition(DefKind::Type),
        "upickle/core/src/upickle/core/Visitor.scala",
        21,
    ),
    (
        "_root_.upickle.core.Visitor",
        NodeKind::Package,
        "upickle/core/src/upickle/core/Visitor.scala",
        155,
    ),
    // ...and a member reached *through* the object, which is what makes the
    // container namespace worth having.
    (
        "_root_.upickle.core.Visitor#Delegate",
        NodeKind::Definition(DefKind::Type),
        "upickle/core/src/upickle/core/Visitor.scala",
        156,
    ),
    // A `package object`'s members are members of the package itself.
    (
        "_root_.ujson#read",
        NodeKind::Definition(DefKind::Function),
        "ujson/src/ujson/package.scala",
        18,
    ),
    // The unnamed package: two files write no `package` clause at all, and
    // they still have a container — the one Scala calls `_root_`.
    (
        "_root_",
        NodeKind::Package,
        "ujson/src/ujson/package.scala",
        1,
    ),
    // The cross-built name, at its third declaration site.
    (
        "_root_.upickle#WebJson",
        NodeKind::Definition(DefKind::Type),
        "upickle/src-native/upickle/WebJson.scala",
        3,
    ),
    // A cross-built `package object` member: one FQN, two Scala versions.
    (
        "_root_.upickle.core.compat#toIterator",
        NodeKind::Definition(DefKind::Function),
        "upickle/core/src-2.12/upickle/core/compat/package.scala",
        32,
    ),
    // An object nested in a package that is itself only ever written as a
    // qualified clause.
    (
        "_root_.upickle.default",
        NodeKind::Package,
        "upickle/src/upickle/Api.scala",
        187,
    ),
    (
        "_root_.upickletest.Flatten",
        NodeKind::Package,
        "upickle/test/src/upickletest/MacroTests.scala",
        148,
    ),
];

#[test]
fn the_scala_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
        return;
    }
    let walked = source_files::<ScalaLang>(corpus).expect("walking the corpus");
    assert_eq!(walked.len(), FILES, "the walk found a different file set");

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_scala(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Scala.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "scala        resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    for (code, count) in &tally.unresolved {
        println!("             {} {count}", reason_name(*code));
        reasons.insert(reason_name(*code).to_string(), *count);
    }

    // -- completeness -----------------------------------------------------

    // Independently re-extracted: the same files the scan owned, read again
    // from disk and put through the extractor with no resolver in sight. The
    // scan's buckets must account for every one of those references and for
    // nothing else.
    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    drop(store);
    assert_eq!(owned.len(), FILES, "the scan owned a different file set");

    let mut re_extracted = 0u64;
    let mut forms: BTreeMap<&str, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        let facts = extract(rel, &source);
        re_extracted += facts.refs.len() as u64;
        for r in &facts.refs {
            // The tier-2 contract, checked on real code and not only on a
            // fixture: a call or type reference here would put references
            // into a denominator this track cannot resolve.
            assert_eq!(r.kind, RefKind::Import, "{rel}: {}", r.raw_target);
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
        }
        // An import selector and its reference are paired by span, so a
        // selector with no reference would be a silently dropped import.
        assert_eq!(
            facts.header.imports.len(),
            facts.refs.len(),
            "{rel}: import selectors and import references disagree",
        );
        for spec in &facts.header.imports {
            *forms.entry(spec.form.name()).or_default() += 1;
        }
        // Every file declares the package its definitions live in, first,
        // whether or not it writes a `package` clause.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no package",
        );
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
        }
    }
    println!("             forms {forms:?}");
    println!("             defs  {kinds:?}");

    let accounted =
        measured.resolved + measured.external + measured.local_binding + measured.unresolved;
    assert_eq!(
        accounted,
        re_extracted,
        "{re_extracted} references were extracted from {} files but {accounted} were accounted \
         for; a resolver that drops a reference reports a better rate for less work",
        owned.len(),
    );

    // -- the definitions, exactly ------------------------------------------

    let want: BTreeMap<u8, u64> = DEFS.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        kinds, want,
        "the definition census moved; tier 2's own deliverable is half \
         definitions and no rate can see them",
    );

    // -- the tally, exactly -----------------------------------------------

    assert_eq!(re_extracted, REFERENCES);
    let want: BTreeMap<&str, u64> = FORMS.iter().copied().collect();
    assert_eq!(forms, want, "the import-form census moved");

    assert_eq!(measured.resolved, 267);
    // Nothing here is `External`, by decision and not by accident: the bucket
    // sits outside both rate terms, so a track that mints none cannot raise
    // its rate by reclassifying.
    assert_eq!(measured.external, 0);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The other bucket outside both rate terms is empty too.
    assert_eq!(measured.local_binding, 0);
    assert_eq!(measured.unresolved, 364);

    // The floor, named, and both halves of it are honest costs rather than
    // bugs:
    //
    // `UnknownPackage` is every path whose first segment binds in no scope
    // this repository declares. Three shapes, and 166 of its 305 rows are
    // the first: the platform roots `scala.*` (105 rows) and `java.*` (61);
    // the Maven artifacts `build.mill` names without stating the package
    // prefixes they ship (`utest`, `io.circe`, `play.api`, `argonaut`,
    // `acyclic`); and the path-dependent imports `import c.universe._` and
    // `import quotes.reflect.*`, which start at a term whose *type* names
    // the container.
    assert_eq!(reasons.get("UnknownPackage").copied(), Some(309));
    // `NoMatchingDefinition` is dominated by one shape, and it is a tier
    // boundary rather than a bug: `import upickle.default.read` names a
    // member `object default` **inherits** from a trait. Placing it needs the
    // supertype relation, which is built from type references — and a type
    // reference is exactly what tier 2 does not emit. Recorded here rather
    // than hidden, because this reason's own definition says it should sit
    // near zero in a corpus that compiles.
    assert_eq!(reasons.get("NoMatchingDefinition").copied(), Some(55));
    assert_eq!(
        reasons.len(),
        2,
        "an unexpected reason appeared: {reasons:?}"
    );

    // -- the definitions the store kept, by kind and by name ---------------

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let mut packages = 0u64;
    let mut externals = 0u64;
    let mut multi_file: BTreeSet<String> = BTreeSet::new();
    read.for_each_node(|_, record| {
        match record {
            NodeRecord::Definition {
                kind,
                fqn,
                declarations,
                ..
            } => {
                *stored.entry(kind).or_default() += 1;
                let files: BTreeSet<&str> = declarations.iter().map(|d| d.file.as_str()).collect();
                if files.len() > 1 {
                    multi_file.insert(fqn);
                }
            }
            NodeRecord::Package { .. } => packages += 1,
            NodeRecord::External { .. } => externals += 1,
        }
        Ok(())
    })
    .expect("walking the node table");
    println!("             nodes {stored:?} packages {packages} externals {externals}");
    println!("             cross-built {multi_file:?}");
    let want: BTreeMap<u8, u64> = STORED.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "the stored definition census moved");
    assert_eq!(packages, PACKAGES, "the stored package census moved");
    assert_eq!(externals, EXTERNALS, "this track mints no external node");

    // -- the union over build configurations -------------------------------

    assert_eq!(
        multi_file.len() as u64,
        COLLISIONS,
        "the cross-build union moved: {multi_file:?}",
    );
    assert_eq!(
        report.fqn_collisions, COLLISIONS,
        "the report and the node table disagree about the cross-build union",
    );
    for one in [
        "_root_.upickle#WebJson",
        "_root_.upickle.core.compat#Factory",
        "_root_.upickle.implicits#MacroImplicits",
    ] {
        assert!(multi_file.contains(one), "{one} is not cross-built");
    }

    // -- the named nodes ---------------------------------------------------

    for (fqn, kind, file, line) in PINNED {
        let id = node_id(Domain::Scala, fqn);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("{fqn} is not in the store"));
        assert_eq!(def.node.name, *fqn);
        assert_eq!(def.node.kind, *kind, "{fqn}");
        let here: Vec<u32> = def
            .declarations
            .iter()
            .filter(|d| d.file == *file)
            .map(|d| d.line)
            .collect();
        assert!(
            here.contains(line),
            "{fqn} is not declared at {file}:{line} — {} site(s) in that file, at {here:?}",
            here.len(),
        );
    }
    drop(read);

    // -- the ratchet ------------------------------------------------------

    let text = std::fs::read_to_string(BASELINE).unwrap_or_else(|e| {
        panic!(
            "reading {BASELINE}: {e}; record it with \
             `arthron gate {CORPUS} --language scala --baseline {BASELINE} --rebase --commit <sha>`"
        )
    });
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Scala.name(),
        "{BASELINE} measures another language; rates are per language and never aggregated",
    );
    assert_eq!(
        baseline.corpus, CORPUS,
        "{BASELINE} was recorded from another corpus",
    );
    match evaluate(&baseline, &measured) {
        GateVerdict::Pass { improved } => {
            if improved {
                println!("gate: pass — improved on the baseline; re-base to move the ratchet");
            }
        }
        GateVerdict::Fail(failures) => {
            let joined: Vec<String> = failures.iter().map(ToString::to_string).collect();
            panic!("gate: FAIL\n  {}", joined.join("\n  "));
        }
        GateVerdict::Error(e) => panic!("gate: error — {e}"),
    }
}
