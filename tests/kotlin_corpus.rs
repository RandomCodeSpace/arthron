//! Acceptance for the Kotlin track against the okio corpus: nothing is
//! dropped, and the measured counts are the ones the committed baseline was
//! recorded from.
//!
//! Three questions; the first and the last are the two every corpus test
//! asks, and the middle one is the half of tier 2 no rate reaches:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** Tier 2's deliverable is definitions, structure and
//!    imports, and the rate can only see the imports. The definition census
//!    is therefore asserted exactly on **both** sides of the store — an
//!    owner-frame bug that lost most of the corpus's methods moves no rate,
//!    no bucket and no baseline, so nothing else here would notice it — and
//!    named nodes are pinned with their kinds and declaration lines, because
//!    a count is not a census.
//! 3. **The ratchet.** The counts are compared against
//!    `baselines/kotlin-okio.toml` through the same [`arthron::gate::evaluate`]
//!    the `arthron gate` command uses, so a rate regression — or drift in
//!    either of the two buckets that sit outside the rate — fails the build.
//!
//! okio is pinned and is never edited, so every number below is a fact about
//! this extractor and this resolver reading a fixed 272 files. A change to
//! any of them is a change in what the track *does*, and must arrive as a
//! deliberate edit here and a deliberate `--rebase` beside it, never as a
//! test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/kotlin/okio --language kotlin \
//!     --baseline baselines/kotlin-okio.toml --rebase --commit 6604edb
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken track.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{
    Counts, FORMAT, GateVerdict, evaluate, is_renderable, parse_baseline, render_baseline,
};
use arthron::model::{DefKind, Domain, Lang, RefKind, node_id, reason_name};
use arthron::pipeline::source_files;
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_kotlin::extract::extract;
use arthron::track_kotlin::lang::KtLang;
use arthron::track_kotlin::resolve::scan_kotlin;

mod support;

const CORPUS: &str = "corpus/kotlin/okio";
const BASELINE: &str = "baselines/kotlin-okio.toml";
/// The pinned corpus revision, for the baseline's provenance line.
const CORPUS_COMMIT: &str = "6604edb";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
const FILES: usize = 272;
const REFERENCES: u64 = 1899;
/// Imports that rename what they bind. The alias is file-local and binds
/// nothing tier 2 resolves; it is in the reference's `raw_target` so that two
/// aliases of one target stay two rows.
const ALIASED: u64 = 12;
/// On-demand imports. okio writes none, which is why
/// [`arthron::track_kotlin::resolve`] records that rule as unexercised.
const ON_DEMAND: u64 = 0;

/// Every package the corpus declares, and how many files declare it.
///
/// Four names over 272 files across three Gradle modules, which is the shape
/// this corpus was chosen for: same-package resolution with no import at all
/// is the common case here. The empty name is the default package, declared
/// by the three `build.gradle.kts` scripts — a container with no name, which
/// is a different fact from a file naming no container.
const PACKAGE_FILES: &[(&str, u64)] = &[
    ("", 3),
    ("okio", 226),
    ("okio.fakefilesystem", 4),
    ("okio.internal", 36),
    ("okio.internal.preview1", 3),
];

/// Every definition the extractor emits over those 272 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. Definitions
/// are the half of tier 2 the import-rate gate cannot see: an owner-frame bug
/// that lost most of the methods in the corpus would leave every rate, every
/// bucket and the whole ratchet untouched. `Module` counts one container per
/// file, written or defaulted.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 409),
    (DefKind::Method, 2969),
    (DefKind::Type, 312),
    (DefKind::Const, 118),
    (DefKind::Constructor, 153),
    (DefKind::Property, 675),
    (DefKind::Module, 272),
    (DefKind::Alias, 11),
];

/// Definition nodes the store holds after merging, by kind.
///
/// Lower than [`DEFS`] everywhere, and for two reasons the corpus was chosen
/// to exercise: one name declared in several source sets is one entity
/// (`expect`/`actual`), and a callable key is a *name* rather than a name
/// plus an arity, so overloads share a node too. The pair of censuses is the
/// point — the extractor's says nothing was lost on the way in, the store's
/// says nothing was lost or over-merged on the way through.
///
/// `Alias` falls 11 → 4 because seven of the corpus's ten `actual typealias`
/// declarations name something an `expect class` declares one source set
/// over, and the merged node records the kind of the first declaration the
/// walk reached. `DefKind::Module` is absent because the driver files a
/// package as a *package* node rather than a definition; those are counted by
/// [`PACKAGES`].
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Function, 252),
    (DefKind::Method, 2175),
    (DefKind::Type, 251),
    (DefKind::Const, 118),
    (DefKind::Constructor, 103),
    (DefKind::Property, 569),
    (DefKind::Alias, 4),
];

/// Package nodes: the four the corpus declares plus the default package the
/// build scripts sit in. 272 module definitions in, 5 identities out, which
/// is what `package okio` written in 226 files across three Gradle modules
/// looks like from the other side.
const PACKAGES: u64 = 5;

/// External nodes, named by the root segment of the import that reached them.
///
/// The coarsest unit this build can name without guessing where a
/// dependency's package begins — `org.junit` and `org.assertj` share `org`,
/// because a Kotlin import states no boundary between the package and the
/// type and no dependency's sources are read here.
const EXTERNALS: &[&str] = &[
    "aQute", "app", "assertk", "com", "java", "javax", "kotlin", "kotlinx", "org", "platform",
];

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. `okio#Path.Companion.toPath()`
/// cannot be right unless a companion object's members are filed under the
/// name an import spells, and `okio#Lock` cannot be right unless one name
/// declared in three source sets is one node.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // The package 226 of the 269 source files declare, and the one an
    // `import okio.…` resolves through.
    (
        "okio",
        NodeKind::Package,
        "okio/src/commonMain/kotlin/okio/Buffer.kt",
        16,
    ),
    (
        "okio.internal",
        NodeKind::Package,
        "okio/src/commonMain/kotlin/okio/internal/Buffer.kt",
        21,
    ),
    // The default package, declared by nothing but the build scripts. Kotlin
    // cannot import from it, so nothing resolves here — it is a container all
    // the same.
    ("", NodeKind::Package, "okio/build.gradle.kts", 1),
    // The chain `import okio.Path.Companion.toPath` walks: a package, a
    // class, its companion object, and a function on it.
    (
        "okio#Path.Companion.toPath()",
        NodeKind::Definition(DefKind::Method),
        "okio/src/commonMain/kotlin/okio/Path.kt",
        307,
    ),
    // `expect class Lock` in commonMain, `actual typealias Lock = ReentrantLock`
    // in jvmMain, `actual class Lock` in nonJvmMain: one FQN, three source
    // sets, one node. The identity space carries no source-set dimension.
    (
        "okio#Lock",
        NodeKind::Definition(DefKind::Type),
        "okio/src/jvmMain/kotlin/okio/-JvmPlatform.kt",
        31,
    ),
    (
        "okio#IOException",
        NodeKind::Definition(DefKind::Type),
        "okio/src/commonMain/kotlin/okio/CommonPlatform.kt",
        35,
    ),
    // A top-level internal function `import okio.internal.commonWrite` names,
    // declared thirteen times across source sets and overloads.
    (
        "okio.internal#commonWrite()",
        NodeKind::Definition(DefKind::Function),
        "okio/src/commonMain/kotlin/okio/internal/Buffer.kt",
        436,
    ),
    // A `const val` on an `object`, which `import okio.TestUtil.SEGMENT_SIZE`
    // names — and which the value sigil keeps apart from a classifier of the
    // same name.
    (
        "okio#TestUtil.SEGMENT_SIZE!",
        NodeKind::Definition(DefKind::Const),
        "okio/src/jvmTest/kotlin/okio/TestUtil.kt",
        33,
    ),
    // The cross-module import: five test files write
    // `import okio.fakefilesystem.FakeFileSystem`, and the module that
    // declares it is vendored for exactly that reason.
    (
        "okio.fakefilesystem#FakeFileSystem",
        NodeKind::Definition(DefKind::Type),
        "okio-fakefilesystem/src/commonMain/kotlin/okio/fakefilesystem/FakeFileSystem.kt",
        68,
    ),
    // The class the pinned grammar mangles: the declaration survives in the
    // two source sets where it does, and its members do not survive anywhere.
    // `import okio.ByteString` resolves; `import okio.ByteString.Companion.encodeUtf8`
    // does not, and says so.
    (
        "okio#ByteString",
        NodeKind::Definition(DefKind::Type),
        "okio/src/jvmMain/kotlin/okio/ByteString.kt",
        61,
    ),
    // An explicit constructor is a node; an implicit one is not.
    (
        "okio#Buffer.<init>()",
        NodeKind::Definition(DefKind::Constructor),
        "okio/src/commonMain/kotlin/okio/Buffer.kt",
        31,
    ),
    // The dependency root 80 of the corpus's files import from.
    (
        "java",
        NodeKind::External,
        "okio/src/jvmMain/kotlin/okio/-JvmPlatform.kt",
        19,
    ),
];

/// Whether the corpus has been cloned in.
fn corpus_present(corpus: &Path) -> bool {
    if corpus.join("okio").is_dir() {
        return true;
    }
    support::missing(corpus);
    false
}

#[test]
fn the_kotlin_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus_present(corpus) {
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_kotlin(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Kotlin.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "kotlin       resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
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
    assert_eq!(
        source_files::<KtLang>(corpus)
            .expect("walking the corpus")
            .len(),
        FILES,
        "the walk and the store disagree about what Kotlin owns",
    );

    let mut re_extracted = 0u64;
    let mut aliased = 0u64;
    let mut on_demand = 0u64;
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    let mut package_files: BTreeMap<String, u64> = BTreeMap::new();
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
            assert!(r.argc.is_none(), "{rel}: {}", r.raw_target);
            let e = r.enclosing.as_ref().expect("an import states its package");
            assert_eq!(e.kind, DefKind::Module, "{rel}: {}", r.raw_target);
            assert_eq!(e.path, std::slice::from_ref(&facts.header.package), "{rel}");
        }
        // An import clause and its reference are paired by span, so a clause
        // with no reference would be a silently dropped import.
        assert_eq!(
            facts.header.imports.len(),
            facts.refs.len(),
            "{rel}: import clauses and import references disagree",
        );
        for (spec, r) in facts.header.imports.iter().zip(&facts.refs) {
            assert_eq!(spec.span, r.span, "{rel}: {}", r.raw_target);
            aliased += u64::from(spec.alias.is_some());
            on_demand += u64::from(spec.on_demand);
        }
        // Every file states the container its definitions live in, whether or
        // not it writes a `package` header.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no container",
        );
        assert_eq!(facts.defs[0].name, facts.header.package, "{rel}");
        *package_files
            .entry(facts.header.package.clone())
            .or_default() += 1;
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
            if d.kind != DefKind::Module {
                assert_eq!(
                    d.owner.first().map(String::as_str),
                    Some(facts.header.package.as_str()),
                    "{rel}: {} states another package",
                    d.name,
                );
            }
        }
    }
    println!("             defs {kinds:?}");
    println!("             packages {package_files:?}");

    // -- the definitions, exactly ------------------------------------------

    let want: BTreeMap<u8, u64> = DEFS.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        kinds, want,
        "the definition census moved; tier 2's own deliverable is half \
         definitions and no rate can see them",
    );
    assert_eq!(
        package_files.into_iter().collect::<Vec<_>>(),
        PACKAGE_FILES
            .iter()
            .map(|(p, n)| ((*p).to_string(), *n))
            .collect::<Vec<_>>(),
        "the package census moved",
    );

    let accounted =
        measured.resolved + measured.external + measured.local_binding + measured.unresolved;
    assert_eq!(
        accounted,
        re_extracted,
        "{re_extracted} references were extracted from {} files but {accounted} were accounted \
         for; a resolver that drops a reference reports a better rate for less work",
        owned.len(),
    );

    // -- the tally, exactly -----------------------------------------------

    assert_eq!(re_extracted, REFERENCES);
    assert_eq!(aliased, ALIASED);
    assert_eq!(
        on_demand, ON_DEMAND,
        "okio writes no on-demand import; the rule for one is recorded as unexercised",
    );

    // Every `import okio…` line names a package this repository declares, so
    // the denominator is exactly those and the external bucket is exactly
    // everything else.
    assert_eq!(measured.resolved, 683);
    assert_eq!(measured.external, 1136);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The bucket that sits outside both rate terms is empty, which is
    // what makes this rate un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    assert_eq!(measured.unresolved, 80);
    assert_eq!(measured.resolved + measured.unresolved, 763);

    // The floor, named, and both buckets count *against* the rate.
    //
    // 68 of the 70 are one grammar defect: tree-sitter-kotlin 0.4.1 cannot
    // parse a comment or a modified primary constructor written on the line
    // after a class header, and `okio.ByteString`'s companion object is
    // inside a body it loses in every source set that declares one. The
    // extractor drops those members rather than emitting them at the top
    // level, so this is a miss and not a wrong definition. The other two are
    // `okio.internal.ErrnoException` and `okio.internal.readString`, declared
    // in a source set the corpus does not vendor.
    assert_eq!(reasons.get("NoMatchingDefinition").copied(), Some(70));
    // `okio.internal.linux` is generated by cinterop from Linux UAPI headers
    // the corpus excludes. `okio.internal` is declared here and holds nothing
    // called `linux`, so the name's own package is one this build never
    // indexed — which is a different fact from this extractor losing a
    // definition, and neither is `External`.
    assert_eq!(reasons.get("UnknownPackage").copied(), Some(10));
    assert_eq!(
        reasons.len(),
        2,
        "an unexpected reason appeared: {reasons:?}"
    );

    // Two declarations sharing an FQN are ordinary here — one name per source
    // set, and one importable name per overload group — so the language calls
    // every one of them one entity.
    assert_eq!(
        report.fqn_collisions, 0,
        "an FQN collision the language does not call one entity",
    );

    // -- the definitions the store kept, by kind and by name ---------------

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let mut packages = 0u64;
    let mut externals: Vec<String> = Vec::new();
    read.for_each_node(|_, record| {
        match record {
            NodeRecord::Definition { kind, .. } => *stored.entry(kind).or_default() += 1,
            NodeRecord::Package { .. } => packages += 1,
            NodeRecord::External { package, .. } => externals.push(package),
        }
        Ok(())
    })
    .expect("walking the node table");
    externals.sort();
    println!("             nodes {stored:?} packages {packages} externals {externals:?}");
    let want: BTreeMap<u8, u64> = STORED.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "the stored definition census moved");
    assert_eq!(packages, PACKAGES, "the stored package census moved");
    assert_eq!(externals, EXTERNALS, "the stored external census moved");

    for (fqn, kind, file, line) in PINNED {
        // An external node's identity carries the `external:` prefix the
        // driver mints it under; every other node's is its FQN as written
        // here.
        let spelled = match kind {
            NodeKind::External => format!("external:{fqn}"),
            _ => (*fqn).to_string(),
        };
        let id = node_id(Domain::Kotlin, &spelled);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("`{fqn}` is not in the store"));
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
            "`{fqn}` is not declared at {file}:{line} — {} site(s) in that file, at {here:?}",
            here.len(),
        );
    }

    // One name, three source sets, one node: the multiplatform question this
    // corpus was chosen to ask. `okio.Lock` is `expect class` in commonMain,
    // `actual typealias` in jvmMain and `actual class` in nonJvmMain, and
    // `import okio.Lock` names one thing whichever platform compiles.
    let lock = definition(&read, &node_id(Domain::Kotlin, "okio#Lock"))
        .expect("reading okio#Lock")
        .expect("okio#Lock is in the store");
    let mut files: Vec<&str> = lock.declarations.iter().map(|d| d.file.as_str()).collect();
    files.sort();
    files.dedup();
    assert_eq!(
        files,
        [
            "okio/src/commonMain/kotlin/okio/CommonPlatform.kt",
            "okio/src/jvmMain/kotlin/okio/-JvmPlatform.kt",
            "okio/src/nonJvmMain/kotlin/okio/NonJvmPlatform.kt",
        ],
        "expect/actual is being read as one declaration or as three nodes",
    );
    // And overloads share the node their importable name owns: nine `div`
    // declarations across three source sets, one `import okio.Path` away.
    let div = definition(&read, &node_id(Domain::Kotlin, "okio#Path.div()"))
        .expect("reading okio#Path.div()")
        .expect("okio#Path.div() is in the store");
    assert_eq!(div.declarations.len(), 9);
    drop(read);

    // -- the ratchet ------------------------------------------------------

    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Kotlin.name(),
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

/// The baseline is written by `arthron gate --rebase` and by nothing else.
#[test]
fn the_kotlin_baseline_names_the_corpus_it_measures() {
    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(baseline.corpus, CORPUS);
    assert_eq!(baseline.commit, CORPUS_COMMIT);
    assert_eq!(baseline.language, Lang::Kotlin.name());
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
