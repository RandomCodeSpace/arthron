//! Milestone acceptance for Java: a real, honest rate on a real corpus, and a
//! ratchet that holds it.
//!
//! The corpus is not vendored here — it lives in `RandomCodeSpace/arthron-corpus`
//! and is cloned into `./corpus` (gitignored). Skipping when it is absent is
//! correct: failing would make an unfetched corpus look like a broken engine.
//!
//! The ratchet is the project's own, reused rather than reimplemented:
//! [`arthron::gate::parse_baseline`] reads the same file format the Go
//! baselines use and [`arthron::gate::evaluate`] performs the same exact
//! integer comparison. It runs here *as well as* through `arthron gate
//! --language java`, which since #10 measures a language other than Go —
//! same file, same comparison, one from the suite and one from CI.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{
    Counts, FORMAT, GateVerdict, evaluate, is_renderable, parse_baseline, render_baseline,
};
use arthron::model::{DefKind, Lang, node_id, reason_name};
use arthron::pipeline::source_files;
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_java::extract::extract;
use arthron::track_java::{JavaLang, scan_java};

mod support;

const CORPUS: &str = "corpus/java/commons-lang";
const BASELINE: &str = "baselines/java-commons-lang.toml";
/// The pinned corpus revision, for the baseline's provenance line.
const CORPUS_COMMIT: &str = "598dfc1";

/// Every Java corpus and the baseline that holds it: `(corpus, baseline,
/// pinned revision)`.
///
/// **Two corpora, two baselines**, for the reason Go has two: a gate against a
/// single repository locks in a number rather than a capability. gson is loud
/// exactly where commons-lang is quiet — generics threaded through
/// `TypeAdapter<T>`, eleven overloads of `fromJson` in one file, and a JPMS
/// `module-info.java` beside the pom, none of which commons-lang has.
const CORPORA: &[(&str, &str, &str)] = &[
    (CORPUS, BASELINE, CORPUS_COMMIT),
    ("corpus/java/gson", "baselines/java-gson.toml", "3ff35d6"),
];

/// Every unresolved reason commons-lang produces, exactly.
///
/// `AmbiguousOverload` dominates because this corpus is overload sets without
/// the argument types to discriminate them, and `NeedsExpressionType` is the
/// honest cost of not running a type checker on `f().m()`. The two are
/// different facts and the gate cannot tell them apart: moving all 9218 of
/// the first into `NoMatchingDefinition` leaves every gated integer where it
/// was.
const COMMONS_LANG_REASONS: &[(&str, u64)] = &[
    ("AmbiguousOverload", 9218),
    ("NeedsExpressionType", 6566),
    ("NeedsTypeInference", 342),
    ("NoMatchingDefinition", 123),
    ("UnindexedSupertype", 30),
];

/// gson's, exactly. The same five reasons in a different mixture — generics
/// threaded through `TypeAdapter<T>` push `NeedsExpressionType` past
/// `AmbiguousOverload`, which commons-lang never does.
const GSON_REASONS: &[(&str, u64)] = &[
    ("AmbiguousOverload", 1282),
    ("NeedsExpressionType", 4713),
    ("NeedsTypeInference", 72),
    ("NoMatchingDefinition", 37),
    ("UnindexedSupertype", 1),
];

/// Whether the corpus has been cloned in.
fn corpus_present(corpus: &Path) -> bool {
    if corpus.join("src/main/java").is_dir() {
        return true;
    }
    support::missing(corpus);
    false
}

/// Count the corpus's references by extracting it again, independently of the
/// pipeline.
///
/// Deliberately not "ask the pipeline how many it found": a reference lost
/// between the extractor and the store would vanish from both sides of the
/// comparison and the assertion would pass. It shares only what it must to be
/// looking at the same corpus at all — [`extract`] and the file walk.
fn extracted_reference_count(corpus: &Path) -> u64 {
    let mut total = 0u64;
    for path in source_files::<JavaLang>(corpus).expect("walking the corpus") {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        total += extract(&rel, &source).refs.len() as u64;
    }
    total
}

#[test]
fn corpus_rate_is_nonzero_and_every_unresolved_has_a_reason() {
    let corpus = Path::new(CORPUS);
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.redb");
    let report = scan_java(corpus, &db).expect("scan");
    let java = &report.per_lang[&Lang::Java.code()];

    let unresolved = java.unresolved_total();
    let rate = arthron::resolution_rate(java.resolved, unresolved)
        .expect("the corpus has references to measure");

    println!("java rate      {rate:.4}");
    println!("  resolved     {}", java.resolved);
    println!("  unresolved   {unresolved}");
    println!("  external     {}", java.external);
    println!("  localbinding {}", java.local_binding);
    for (code, count) in &java.unresolved {
        println!("  {:<22} {count}", reason_name(*code));
    }
    println!("  fqn_collisions {}", report.fqn_collisions);

    // Every reference the extractor produced has exactly one stored outcome:
    // nothing is dropped between the two halves of the scan.
    let store = Store::open(&db).expect("store opens");
    let rows = store.snapshot().expect("snapshot");
    let stored: u64 = rows.rows.values().map(|r| u64::from(r.count)).sum();
    assert_eq!(
        stored,
        extracted_reference_count(corpus),
        "a reference was lost between extraction and the store",
    );

    // A rate of zero is a measurement, but not an acceptable one for a
    // language whose resolver claims to link anything at all.
    assert!(rate > 0.0, "nothing resolved");
    // Rates are per language and never aggregated: Go must not appear in a
    // report produced by the Java track's own scan of a Java-only tree.
    assert!(!report.per_lang.contains_key(&Lang::Go.code()));

    // The half of this test's name that nothing checked. `unresolved_total`
    // is the sum of these buckets, so comparing the two would be an
    // identity; what is worth asserting is that every bucket names a reason
    // a person can read, that none of them is an empty bucket recorded
    // anyway, and that the reasons this corpus must produce are among them.
    assert!(
        !java.unresolved.is_empty(),
        "unresolved with no reason at all"
    );
    for (code, count) in &java.unresolved {
        assert_ne!(
            reason_name(*code),
            "Unknown",
            "reason code {code} names no variant, so {count} references are \
             unresolved for a reason nothing can read",
        );
        assert!(
            *count > 0,
            "{} is recorded with a count of zero: a reason nobody produced",
            reason_name(*code),
        );
    }
    // The floors, named. `NeedsExpressionType` is the honest cost of not
    // running a type checker on `f().m()`, and `AmbiguousOverload` of not
    // having the argument types to discriminate commons-lang's overload
    // sets. A scan reporting neither would have routed them into `external`
    // or `local_binding`, which are outside both terms of the rate — which
    // is how this number could rise without anything being linked.
    for floor in ["NeedsExpressionType", "AmbiguousOverload"] {
        assert!(
            java.unresolved
                .iter()
                .any(|(code, n)| reason_name(*code) == floor && *n > 0),
            "no {floor} floor: it was reclassified, not resolved",
        );
    }
    // And the whole tally, exactly, which is the half the floors above could
    // not reach: a floor holds while 9056 of these 19093 references are
    // relabelled, and every number this file otherwise gates stays put.
    support::assert_reasons(CORPUS, &java.unresolved, COMMONS_LANG_REASONS);
}

// -- the definition census -------------------------------------------------
//
// Java is tier 1 and its rate is over call sites, so it reaches further into
// a file than a tier-2 import rate does — and it still cannot see a
// definition. Deleting the rule that emits `DefKind::Method` takes 9433
// nodes out of commons-lang without moving `resolved`, `unresolved`,
// `external`, `local_binding` or either baseline: every reference that named
// one of them simply changes reason, and the reason tallies are not asserted
// exactly here. So the definitions get their own exact census, on both sides
// of the store, with named nodes beside it.

/// The measurement one Java corpus's census is.
struct Census {
    files: usize,
    defs: &'static [(DefKind, u64)],
    stored: &'static [(DefKind, u64)],
    packages: u64,
    externals: u64,
    pinned: &'static [(&'static str, NodeKind, &'static str, u32)],
}

/// commons-lang: 527 files, and the quiet corpus — no generics threaded
/// through a type parameter, no module descriptor.
const COMMONS_LANG: Census = Census {
    files: 527,
    // `Module` is one per file: every file states the package its
    // definitions live in. `Alias` is the overload key — one entry per
    // `name/arity` that two or more signatures answer to — which is how a
    // call site with three arguments finds a set rather than a guess.
    defs: &[
        (DefKind::Method, 9433),
        (DefKind::Type, 957),
        (DefKind::Constructor, 1006),
        (DefKind::Field, 2199),
        (DefKind::Module, 527),
        (DefKind::Alias, 390),
    ],
    // Seven methods lower: a signature written identically in two files of
    // one package is one identity. `Module` is absent because a package is
    // filed as a package node, counted below.
    stored: &[
        (DefKind::Method, 9426),
        (DefKind::Type, 957),
        (DefKind::Constructor, 1006),
        (DefKind::Field, 2199),
        (DefKind::Alias, 390),
    ],
    packages: 20,
    externals: 47,
    pinned: &[
        (
            "org.apache.commons.lang3#StringUtils",
            NodeKind::Definition(DefKind::Type),
            "src/main/java/org/apache/commons/lang3/StringUtils.java",
            125,
        ),
        // An overload, spelled by its parameter types: `abbreviate` is
        // written four times in this file and only the signature separates
        // them.
        (
            "org.apache.commons.lang3#StringUtils.abbreviate(String,int,int)",
            NodeKind::Definition(DefKind::Method),
            "src/main/java/org/apache/commons/lang3/StringUtils.java",
            274,
        ),
        // And the arity key beside it, which is what a three-argument call
        // site actually probes.
        (
            "org.apache.commons.lang3#StringUtils.abbreviate/3",
            NodeKind::Definition(DefKind::Alias),
            "src/main/java/org/apache/commons/lang3/StringUtils.java",
            274,
        ),
        (
            "org.apache.commons.lang3#StringUtils.CR",
            NodeKind::Definition(DefKind::Field),
            "src/main/java/org/apache/commons/lang3/StringUtils.java",
            182,
        ),
        (
            "org.apache.commons.lang3#StringUtils.<init>/0",
            NodeKind::Definition(DefKind::Constructor),
            "src/main/java/org/apache/commons/lang3/StringUtils.java",
            9211,
        ),
        (
            "org.apache.commons.lang3.tuple",
            NodeKind::Package,
            "src/main/java/org/apache/commons/lang3/tuple/ImmutablePair.java",
            17,
        ),
    ],
};

/// gson: 209 files, and the loud one — `TypeAdapter<T>`, eleven `fromJson`
/// overloads in a single file, and a JPMS `module-info.java`.
const GSON: Census = Census {
    files: 209,
    defs: &[
        (DefKind::Method, 2509),
        (DefKind::Type, 587),
        (DefKind::Constructor, 617),
        (DefKind::Field, 912),
        (DefKind::Module, 209),
        (DefKind::Alias, 30),
    ],
    stored: &[
        (DefKind::Method, 2509),
        (DefKind::Type, 587),
        (DefKind::Constructor, 617),
        (DefKind::Field, 912),
        (DefKind::Alias, 30),
    ],
    // Fourteen: thirteen packages plus the JPMS descriptor, which declares a
    // module and no package and is the one commons-lang has nothing like.
    packages: 14,
    // 34 and not 36. `TypeTokenTest` declares `Outer` and `Enclosing<T>` as
    // method-local classes (JLS §14.3), and `Outer.NonStaticInner` and
    // `Enclosing<T>.Inner` used to escape the narrow local rule because their
    // targets are two segments long — leaving two external nodes claiming
    // that packages named `Outer` and `Enclosing` exist outside this
    // repository. Under the root-binding rule they are `LocalBinding`, which
    // is true, and the two nodes are gone. Nothing else stopped being reached.
    externals: 34,
    pinned: &[
        (
            "com.google.gson#Gson",
            NodeKind::Definition(DefKind::Type),
            "src/main/java/com/google/gson/Gson.java",
            135,
        ),
        // One of the eleven `fromJson` overloads, named by the erasure of
        // its parameters — a generic parameter is written as it appears.
        (
            "com.google.gson#Gson.fromJson(JsonElement,Class<T>)",
            NodeKind::Definition(DefKind::Method),
            "src/main/java/com/google/gson/Gson.java",
            1136,
        ),
        (
            "com.google.gson#Gson.<init>/0",
            NodeKind::Definition(DefKind::Constructor),
            "src/main/java/com/google/gson/Gson.java",
            222,
        ),
        (
            "com.google.gson#Gson.JSON_NON_EXECUTABLE_PREFIX",
            NodeKind::Definition(DefKind::Field),
            "src/main/java/com/google/gson/Gson.java",
            137,
        ),
        (
            "com.google.gson#TypeAdapter",
            NodeKind::Definition(DefKind::Type),
            "src/main/java/com/google/gson/TypeAdapter.java",
            121,
        ),
        // The module descriptor: a container that is not a package, and
        // whose identity says so.
        (
            "module:com.google.gson",
            NodeKind::Package,
            "src/main/java/module-info.java",
            22,
        ),
    ],
};

/// Count the definitions on both sides of the store and compare them with
/// what this corpus's [`Census`] records.
fn assert_census(corpus: &str, census: &Census) {
    let root = Path::new(corpus);
    if !corpus_present(root) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.redb");
    scan_java(root, &db).expect("scan");

    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    drop(store);
    assert_eq!(
        owned.len(),
        census.files,
        "{corpus}: the scan owned a different file set",
    );

    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(root.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        for def in &extract(rel, &source).defs {
            *kinds.entry(def.kind.code()).or_default() += 1;
        }
    }
    println!("{corpus}: extracted defs {kinds:?}");
    let want: BTreeMap<u8, u64> = census.defs.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        kinds, want,
        "{corpus}: the definition census moved, and no rate can see it",
    );

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let (mut packages, mut externals) = (0u64, 0u64);
    read.for_each_node(|_, record| {
        match record {
            NodeRecord::Definition { kind, .. } => *stored.entry(kind).or_default() += 1,
            NodeRecord::Package { .. } => packages += 1,
            NodeRecord::External { .. } => externals += 1,
        }
        Ok(())
    })
    .expect("walking the node table");
    println!("{corpus}: stored defs {stored:?} packages {packages} externals {externals}");
    let want: BTreeMap<u8, u64> = census.stored.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "{corpus}: the stored definition census moved");
    assert_eq!(
        packages, census.packages,
        "{corpus}: the stored package census moved",
    );
    assert_eq!(
        externals, census.externals,
        "{corpus}: the stored external census moved",
    );

    for (fqn, kind, file, line) in census.pinned {
        let id = node_id(Lang::Java.domain(), fqn);
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
}

#[test]
fn the_commons_lang_definition_census_is_exact() {
    assert_census(CORPUS, &COMMONS_LANG);
}

#[test]
fn the_gson_definition_census_is_exact() {
    assert_census(CORPORA[1].0, &GSON);
}

/// Measure the corpus once against a cold store.
///
/// The ratchet and the recorder share it so that the file one writes is the
/// number the other compares: two measurement paths would let a baseline be
/// recorded from a scan the gate never performs.
fn measure(corpus: &Path) -> (Counts, BTreeMap<u8, u64>) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let report = scan_java(corpus, &dir.path().join("graph.redb")).expect("scan");
    let java = &report.per_lang[&Lang::Java.code()];
    (
        Counts {
            resolved: java.resolved,
            external: java.external,
            local_binding: java.local_binding,
            unresolved: java.unresolved_total(),
        },
        java.unresolved.clone(),
    )
}

fn assert_ratchet(corpus: &str, baseline_path: &str, reasons: &[(&str, u64)]) {
    let root = Path::new(corpus);
    if !corpus_present(root) {
        return;
    }
    let text = std::fs::read_to_string(baseline_path)
        .unwrap_or_else(|e| panic!("reading {baseline_path}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{baseline_path}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Java.name(),
        "{baseline_path} measures another language; rates are per language and never aggregated",
    );
    assert_eq!(
        baseline.corpus, corpus,
        "{baseline_path} was recorded from another corpus",
    );

    let (measured, unresolved) = measure(root);
    println!(
        "{corpus}: resolved {} external {} local-binding {} unresolved {}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    for (code, count) in &unresolved {
        println!("  {}: {count}", reason_name(*code));
    }
    // Exactly, not as a floor. `evaluate` below compares four integers and
    // none of them moves when an unresolved reference is relabelled, so this
    // is the only thing standing between a resolver and 9056 references
    // reported as "there is no such definition" when the truth is "there are
    // several and I cannot choose".
    support::assert_reasons(corpus, &unresolved, reasons);

    match evaluate(&baseline, &measured) {
        GateVerdict::Pass { .. } => {}
        other => panic!("{baseline_path}: {other:?}\nmeasured {measured:?}"),
    }
}

#[test]
fn the_ratchet_holds() {
    assert_ratchet(CORPUS, BASELINE, COMMONS_LANG_REASONS);
}

#[test]
fn the_gson_ratchet_holds() {
    let (corpus, baseline, _) = CORPORA[1];
    assert_ratchet(corpus, baseline, GSON_REASONS);
}

/// The baseline is written by `arthron gate --rebase` and by nothing else.
///
/// There was a second writer here — an `#[ignore]`d test that measured the
/// corpus and rendered the file itself — from when `arthron gate` could only
/// measure Go. #10 gave the command `--language`, so the reason is gone and
/// the risk is not: two writers can disagree, and the one that wrote the file
/// would not be the one the gate compares with. The command is:
///
/// ```text
/// arthron gate corpus/java/commons-lang --language java \
///     --baseline baselines/java-commons-lang.toml --rebase --commit 598dfc1
/// arthron gate corpus/java/gson --language java \
///     --baseline baselines/java-gson.toml --rebase --commit 3ff35d6
/// ```
#[test]
fn every_java_baseline_names_the_corpus_it_measures() {
    for (corpus, baseline_path, commit) in CORPORA {
        let text = std::fs::read_to_string(baseline_path)
            .unwrap_or_else(|e| panic!("reading {baseline_path}: {e}"));
        let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{baseline_path}: {e}"));
        assert_eq!(baseline.corpus, *corpus);
        assert_eq!(baseline.commit, *commit);
        assert_eq!(baseline.language, Lang::Java.name());
        assert_eq!(baseline.format, FORMAT);
        for value in [&baseline.corpus, &baseline.commit, &baseline.language] {
            assert!(
                is_renderable(value),
                "provenance `{value}` cannot be written"
            );
        }
        // The reader and the writer agree, which is what makes a rebased file
        // readable by the gate that will compare against it.
        assert_eq!(
            parse_baseline(&render_baseline(&baseline)).expect("a rendered baseline parses"),
            baseline,
        );
    }
}
