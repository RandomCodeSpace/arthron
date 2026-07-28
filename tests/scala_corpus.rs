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
//!    graph than the repository has. Containers are in that count: an
//!    `object` shares `DefKind::Module` with a package, and one filed as a
//!    package node would keep both its declaration sites and still count
//!    nothing, because a package several files declare is not a collision.
//!    Both sides of that rule are asserted by name here.
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

mod support;

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
/// `DefKind::Module` is present and counts **objects only**. Scala files an
/// `object` and a package under one kind, and `Resolver::stores_as_package`
/// is where they part: a package is a namespace every file under it reopens
/// and becomes a package node ([`PACKAGES`]), an `object` is a term written
/// once and becomes a definition — which is what puts a cross-built one into
/// [`COLLISIONS`] instead of merging it into a package node that counts
/// nothing.
///
/// `Type` is 447 rather than the 438 the previous split recorded, and the
/// nine are named in the test body: a companion `class` and `object` nested
/// *inside* a type share one FQN, because the grammar spends its single `#`
/// crossing on the enclosing type and dots every segment below it. The pair
/// merges into one node whose kind is its first declaring site's — a
/// property of the one-crossing grammar, not of this census, and one both
/// sites of the pair are recorded on.
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Function, 534),
    (DefKind::Method, 686),
    (DefKind::Type, 447),
    (DefKind::Const, 283),
    (DefKind::Var, 40),
    (DefKind::Constructor, 14),
    (DefKind::Module, 348),
];

/// Package nodes: every package this repository declares, and nothing else.
///
/// Eighteen — the seventeen named packages plus the unnamed root package two
/// files sit in. Small on purpose: a package node is the one record several
/// files may declare without that being a collision, so anything filed here
/// that is *not* a package is a cross-build duplicate the report can never
/// count. `package upickle.core`, written in twenty-seven files, is one
/// identity here; the 357 `object` identities that used to land beside it are
/// definitions now — 348 of them under `Module` in [`STORED`], and nine
/// merged into the nested companion types described there.
const PACKAGES: u64 = 18;

/// External nodes. **Zero, and asserted rather than observed.** Scala's
/// platform roots and its build's Maven coordinates are both outside this
/// repository and neither can be named here without guessing, so every path
/// that leaves the repository counts against the rate instead of leaving its
/// denominator. See `track_scala::resolve` for the argument.
const EXTERNALS: u64 = 0;

/// Distinct FQNs a definition in more than one file claims.
///
/// The union over build configurations, counted — **containers included**.
/// `upickle.WebJson` is written in `src-js`, `src-jvm` and `src-native`;
/// `upickle.core.compat.Factory` in `src-2.12` and `src-2.13+`; the `object
/// upickle.core.compat.SortInPlace` that carries the second of those is in
/// both roots too, and is counted on its own line rather than folded into
/// the package node its kind would otherwise have put it in. Every one is
/// real under its own build and none is merged, because merging would report
/// a cleaner graph than the repository has.
///
/// A *package* is deliberately not here: `upickle.core` is declared by
/// twenty-seven files and that is what a package is, not a collision.
const COLLISIONS: u64 = 26;

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. A companion `trait
/// Visitor` and `object Visitor` cannot both be right unless the term and
/// type namespaces stayed apart, and `ujson.read` cannot be right unless a
/// `package object`'s members landed in the package.
///
/// The kind column is load-bearing twice over. An `object` is
/// `Definition(Module)` and a package is `Package`, and the two records are
/// not interchangeable: only the first can be a [`COLLISIONS`] entry, so an
/// `object` filed as a package is a cross-build duplicate nothing counts.
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
        NodeKind::Definition(DefKind::Module),
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
        NodeKind::Definition(DefKind::Module),
        "upickle/src/upickle/Api.scala",
        187,
    ),
    (
        "_root_.upickletest.Flatten",
        NodeKind::Definition(DefKind::Module),
        "upickle/test/src/upickletest/MacroTests.scala",
        148,
    ),
    // The package that holds them, at one of its twenty-seven sites: still a
    // package node, and still not a collision however many files reopen it.
    (
        "_root_.upickle.core",
        NodeKind::Package,
        "upickle/core/src/upickle/core/Visitor.scala",
        1,
    ),
    // A cross-built **object**, at its second source root. The corpus
    // provenance names this one as the case the corpus exists to expose, and
    // filing it as a package node is what used to make it uncountable.
    (
        "_root_.upickle.core.compat.SortInPlace",
        NodeKind::Definition(DefKind::Module),
        "upickle/core/src-2.13+/upickle/core/compat/SortInPlace.scala",
        3,
    ),
];

/// Companion pairs nested *inside* a type, which the FQN grammar merges.
///
/// The grammar spends its one `#` crossing on the enclosing type, so a
/// `class X` and an `object X` written inside `trait T` are both
/// `…#T.X` and become one node with two declaration sites in one file.
/// Named here rather than left in a census delta: it is a known cost of the
/// one-crossing grammar, it is what makes [`STORED`]'s `Type` count nine
/// higher than the object/package split alone, and a change to the grammar
/// must move this list deliberately. Same file both sites, so none of these
/// is a [`COLLISIONS`] entry — and none should become one.
const NESTED_COMPANIONS: &[&str] = &[
    "_root_.upickle#LegacyApi.TaggedReaderState",
    "_root_.upickle.core#Types.ReadWriter",
    "_root_.upickle.core#Types.Reader",
    "_root_.upickle.core#Types.TaggedReadWriter",
    "_root_.upickle.core#Types.TaggedReader",
    "_root_.upickle.core#Types.TaggedWriter",
    "_root_.upickle.core#Types.Writer",
    "_root_.upickletest#MixedIn.Trt1.ClsA",
    "_root_.upickletest#MixedIn.Trt2.ClsB",
];

#[test]
fn the_scala_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        support::missing(corpus);
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
    // this repository declares. The count asserted below is 309
    // *occurrences* over 305 deduplicated rows — a row is one edge source
    // naming one target, and four of them are written twice in their own
    // file. Three shapes, and 166 of the 305 rows are the first: the
    // platform roots `scala.*` (105 rows) and `java.*` (61); the Maven
    // artifacts `build.mill` names without stating the package prefixes they
    // ship (`utest`, `io.circe`, `play.api`, `argonaut`, `acyclic`); and the
    // 45 path-dependent rows `import c.universe._` and `import
    // quotes.reflect.*`, which start at a term whose *type* names the
    // container. The last shape names no package, so the reason is wrong
    // about it — wrong in the direction that costs the rate, since every one
    // of the 45 counts against it, and `track_scala::resolve` records why
    // naming them properly is a tier-1 question.
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
    let mut multi_file_packages: BTreeSet<String> = BTreeSet::new();
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
            NodeRecord::Package {
                import_path,
                declarations,
                ..
            } => {
                packages += 1;
                let files: BTreeSet<&str> = declarations.iter().map(|d| d.file.as_str()).collect();
                if files.len() > 1 {
                    multi_file_packages.insert(import_path);
                }
            }
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
        // Containers, and the reason this list is 26 and not 17: an `object`
        // written once per build configuration is a *declaration* several
        // files make, exactly as the type beside it is. Filed as a package
        // node it would carry both sites and count nothing, and
        // `upickle.core.compat.SortInPlace` — the name the corpus
        // provenance calls out — would be the one it hid.
        "_root_.upickle.core.compat.SortInPlace",
        "_root_.upickle.core.compat.DistinctBy",
        "_root_.upickle.core.compat.LinkedHashMapCompat",
        "_root_.upickletest.Main",
        "_root_.upickle.withTimeout",
        "_root_.upickletest.withTimeout",
        "_root_.ujson.DoubleToDecimalElem",
        "_root_.ujson.FloatToDecimalElem",
        "_root_.ujson.MathUtilsElem",
    ] {
        assert!(multi_file.contains(one), "{one} is not cross-built");
    }

    // The other side of the same rule: a package node really is declared by
    // every file under it, and that is not a collision. Pinned by name, so
    // that a container wrongly filed as a package shows up here as an
    // *addition* rather than as a silent subtraction from `COLLISIONS`.
    println!("             multi-file packages {multi_file_packages:?}");
    let want: BTreeSet<String> = [
        "_root_",
        "_root_.ujson",
        "_root_.upack",
        "_root_.upickle",
        "_root_.upickle.core",
        "_root_.upickle.core.compat",
        "_root_.upickle.implicits",
        "_root_.upickle.implicits.internal",
        "_root_.upickle.implicits.namedTuples",
        "_root_.upickle.jsonschema",
        "_root_.upickletest",
        "_root_.upickletest.example",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(
        multi_file_packages, want,
        "a node several files declare is filed as a package and counts as no \
         collision; every one of them must be a package",
    );

    // The one merge the FQN grammar does make, named rather than left in a
    // census delta. Both sites are in one file, so none of these is — or may
    // become — a `COLLISIONS` entry.
    for fqn in NESTED_COMPANIONS {
        let id = node_id(Domain::Scala, fqn);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("{fqn} is not in the store"));
        let files: BTreeSet<&str> = def.declarations.iter().map(|d| d.file.as_str()).collect();
        assert_eq!(
            def.declarations.len(),
            2,
            "{fqn} is not the companion pair this list records",
        );
        assert_eq!(files.len(), 1, "{fqn} is cross-built, not a nested pair");
        assert!(
            !multi_file.contains(*fqn),
            "{fqn} became a cross-build collision",
        );
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
