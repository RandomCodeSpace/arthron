//! Acceptance for the Dart track against the collection corpus: nothing is
//! dropped, and the measured counts are the ones the committed baseline was
//! recorded from.
//!
//! Four questions; the first and the last are the two every corpus test here
//! asks, and the middle two are what a *best-effort* tier-2 rate cannot see by
//! itself:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** Tier 2's deliverable is definitions, structure and
//!    imports, and the rate can only see the imports. The definition census is
//!    therefore asserted exactly on both sides of the store — an owner-frame
//!    bug that lost most of the corpus's methods moves no rate, no bucket and
//!    no baseline, so nothing else here would notice it.
//! 3. **The shape of the denominator.** This corpus's rate is high and 49 of
//!    its 124 references sit outside both of the rate's terms, so *which* ones
//!    do is the number that has to be pinned, not just how many. Every URI is
//!    classified by scheme and counted, and the 21 that name this repository's
//!    own package are asserted to be **inside** the rate — laundering those
//!    into `External` would leave the printed rate at 100% while quietly
//!    removing a fifth of what it measures.
//! 4. **The ratchet.** The counts are compared against
//!    `baselines/dart-collection.toml` through the same [`arthron::gate::evaluate`]
//!    the `arthron gate` command uses, so a rate regression — or drift in
//!    either of the two buckets that sit outside the rate — fails the build.
//!
//! Beside the ratchet sits the tally itself, restated. collection is pinned
//! and is never edited, so every number below is a fact about this extractor
//! and this resolver reading a fixed 49 files; a change to any of them is a
//! change in what the track *does*, and must arrive as a deliberate edit here
//! and a deliberate `--rebase` beside it, never as a test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/dart/collection --language dart \
//!     --baseline baselines/dart-collection.toml --rebase --commit <sha>
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken track.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Domain, Lang, RefKind, node_id, reason_name};
use arthron::query::{NodeKind, definition, references};
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_dart::extract::{UriForm, extract};
use arthron::track_dart::lang::library_fqn;
use arthron::track_dart::resolve::scan_dart;

const CORPUS: &str = "corpus/dart/collection";
const BASELINE: &str = "baselines/dart-collection.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
const FILES: usize = 49;
const REFERENCES: u64 = 124;
const IMPORTS: u64 = 96;
const EXPORTS: u64 = 28;

/// Names a `show`/`hide` combinator lists across the corpus.
///
/// Structure, and deliberately **not** references: a combinator names
/// declarations inside another library, and pricing one means computing that
/// library's exported name set through every barrel it re-exports. Counted
/// here so the size of what is not counted is on the record.
const COMBINATOR_NAMES: u64 = 21;

/// Every URI in the corpus, by what its scheme says about where to look.
///
/// The shape of the denominator, pinned. `own-package` and `relative` are the
/// 75 references *inside* the rate; `sdk` and `declared-package` are the 49
/// outside it. Moving one from the first group to the second raises no printed
/// rate and shrinks what the rate measures, which is the one way this number
/// can be gamed.
const SCHEMES: &[(&str, u64)] = &[
    // `dart:collection` ×19, `dart:math` ×7, `dart:typed_data`, `dart:mirrors`
    // — 28 imports plus the one `export 'dart:collection'` in
    // `lib/src/unmodifiable_wrappers.dart`.
    ("sdk", 29),
    // `package:collection/…`, every one of them in `test/`: not one file under
    // `lib/` addresses a sibling that way.
    ("own-package", 21),
    // `package:test/…`, declared by `pubspec.yaml`'s `dev_dependencies`.
    //
    // Which of the two `package:` buckets a URI lands in is decided by an
    // ordering this corpus cannot exercise: `pubspec.yaml` does not declare
    // `collection` as a dependency of itself, so testing the dependency table
    // first would move nothing here. That ordering is fixture-proven instead —
    // `our_own_package_uri_naming_no_file_misses_rather_than_leaving_the_repository`
    // in `tests/dart_resolve.rs` — and this census is what would notice if a
    // future corpus did exercise it.
    ("declared-package", 20),
    // 27 imports and 27 exports, the whole of `lib/`'s internal wiring.
    ("relative", 54),
];

/// Every definition the extractor emits over those 49 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. Definitions
/// are the half of tier 2 the import-rate gate cannot see: an owner-frame bug
/// that lost most of the methods in the corpus would leave every rate, every
/// bucket and the whole ratchet untouched. `Module` counts the 49 synthetic
/// library nodes, one per file.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 108),
    (DefKind::Method, 407),
    (DefKind::Type, 78),
    (DefKind::Const, 8),
    (DefKind::Var, 3),
    (DefKind::Constructor, 84),
    (DefKind::Field, 77),
    (DefKind::Property, 109),
    (DefKind::Module, 49),
];

/// Definition nodes the store holds after merging, by kind.
///
/// Lower than [`DEFS`] in exactly one place: five `Property` declarations are
/// the second half of a member whose first half is already a node — four
/// getter/setter pairs, and `ListSlice.length`, a `final` field with an
/// explicit setter. The pair of censuses is the point — the extractor's says
/// nothing was lost on the way in, the store's says nothing was lost or
/// over-merged on the way through.
///
/// `DefKind::Module` is absent because the driver files a module as a
/// *package* node rather than a definition; those are counted by [`PACKAGES`].
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Function, 108),
    (DefKind::Method, 407),
    (DefKind::Type, 78),
    (DefKind::Const, 8),
    (DefKind::Var, 3),
    (DefKind::Constructor, 84),
    (DefKind::Field, 77),
    (DefKind::Property, 104),
];

/// Package nodes: one library per file, and nothing else. Dart has no
/// namespace above the file, so the count is the file count exactly — any
/// other number would mean two files sharing a library identity.
const PACKAGES: u64 = 49;

/// External nodes: the four SDK libraries the corpus names, plus the one
/// package `pubspec.yaml` declares. Named in [`PINNED`], because which
/// dependency ships a URI is a claim about the outside world and not a count.
const EXTERNALS: u64 = 5;

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. Two `equality` libraries
/// cannot both be right unless the path roots every identity, and
/// `QueueList.[]` cannot be right unless an operator's symbol is read as its
/// name.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // Three libraries, one of which is a barrel and one of which the barrel
    // re-exports. Dart has no namespace above the file, so `equality` twice is
    // two identities and merging them would be the whole grammar failing.
    (
        "$lib/collection.dart",
        NodeKind::Package,
        "lib/collection.dart",
        1,
    ),
    (
        "$lib/equality.dart",
        NodeKind::Package,
        "lib/equality.dart",
        1,
    ),
    (
        "$lib/src/equality.dart",
        NodeKind::Package,
        "lib/src/equality.dart",
        1,
    ),
    (
        "$lib/src/wrappers.dart::DelegatingList",
        NodeKind::Definition(DefKind::Type),
        "lib/src/wrappers.dart",
        151,
    ),
    (
        "$lib/src/wrappers.dart::DelegatingList.add",
        NodeKind::Definition(DefKind::Method),
        "lib/src/wrappers.dart",
        183,
    ),
    // A library-private type: `_` is Dart's visibility and it is still a node.
    (
        "$lib/src/wrappers.dart::_DelegatingIterableBase",
        NodeKind::Definition(DefKind::Type),
        "lib/src/wrappers.dart",
        14,
    ),
    // The name `lib/collection.dart` re-exports under a `show` filter. The
    // filter is not resolved here; the declaration behind it is still a node.
    (
        "$lib/src/algorithms.dart::binarySearch",
        NodeKind::Definition(DefKind::Function),
        "lib/src/algorithms.dart",
        21,
    ),
    (
        "$lib/src/algorithms.dart::mergeSort",
        NodeKind::Definition(DefKind::Function),
        "lib/src/algorithms.dart",
        214,
    ),
    // An operator is a method whose name is its symbol.
    (
        "$lib/src/queue_list.dart::QueueList.[]",
        NodeKind::Definition(DefKind::Method),
        "lib/src/queue_list.dart",
        191,
    ),
    // Both constructors of one class, under Dart's own tear-off spelling: the
    // unnamed one is `.new` and cannot collide with a method, because `new` is
    // a reserved word.
    (
        "$lib/src/canonicalized_map.dart::CanonicalizedMap.new",
        NodeKind::Definition(DefKind::Constructor),
        "lib/src/canonicalized_map.dart",
        28,
    ),
    (
        "$lib/src/canonicalized_map.dart::CanonicalizedMap.from",
        NodeKind::Definition(DefKind::Constructor),
        "lib/src/canonicalized_map.dart",
        42,
    ),
    // An `extension` is a named type declaration like any other.
    (
        "$lib/src/iterable_extensions.dart::IterableExtension",
        NodeKind::Definition(DefKind::Type),
        "lib/src/iterable_extensions.dart",
        20,
    ),
    // The two merges, from both sides. A getter and a setter of one name are
    // one `Property`; a `final` field and an explicit setter are one `Field`,
    // because a final field declares only a getter and Dart allows the setter
    // beside it. Both sites are asserted below.
    (
        "$lib/src/queue_list.dart::QueueList.length",
        NodeKind::Definition(DefKind::Property),
        "lib/src/queue_list.dart",
        159,
    ),
    (
        "$lib/src/list_extensions.dart::ListSlice.length",
        NodeKind::Definition(DefKind::Field),
        "lib/src/list_extensions.dart",
        351,
    ),
    // The one package `pubspec.yaml` declares, and one of the four SDK
    // libraries. `dart:collection` and the package this repository *is* are
    // both called `collection`, and naming the external node after the whole
    // URI is what keeps them two nodes.
    ("test", NodeKind::External, "test/algorithms_test.dart", 12),
    (
        "dart:collection",
        NodeKind::External,
        "lib/src/boollist.dart",
        5,
    ),
    (
        "dart:mirrors",
        NodeKind::External,
        "test/wrapper_test.dart",
        12,
    ),
];

/// Both declaration sites of the two members that merge.
const MERGED: &[(&str, &str, &[u32])] = &[
    (
        "$lib/src/queue_list.dart::QueueList.length",
        "lib/src/queue_list.dart",
        &[159, 162],
    ),
    (
        "$lib/src/list_extensions.dart::ListSlice.length",
        "lib/src/list_extensions.dart",
        &[351, 455],
    ),
];

/// What a URI's scheme says about where the lookup happens.
///
/// A copy of the resolver's own split, deliberately written out here rather
/// than called: a test that asked the resolver which bucket a URI is in would
/// agree with it by construction, and this one has to disagree when the
/// resolver changes.
fn scheme_of(spec: &str) -> &'static str {
    if let Some(rest) = spec.strip_prefix("package:") {
        let name = rest.split('/').next().unwrap_or("");
        return if name == "collection" {
            "own-package"
        } else {
            "declared-package"
        };
    }
    if spec.starts_with("dart:") {
        return "sdk";
    }
    "relative"
}

#[test]
fn the_dart_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_dart(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Dart.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "dart         resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
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
    let mut ref_kinds: BTreeMap<&str, u64> = BTreeMap::new();
    let mut schemes: BTreeMap<&str, u64> = BTreeMap::new();
    let mut dynamic = 0u64;
    let mut combinators = 0u64;
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        let facts = extract(rel, &source);
        re_extracted += facts.refs.len() as u64;
        for r in &facts.refs {
            // The tier-2 contract, checked on real code and not only on a
            // fixture: a call or type reference here would put references into
            // a denominator this track cannot resolve.
            assert!(
                matches!(r.kind, RefKind::Import | RefKind::Export),
                "{rel}: {} is a {:?}",
                r.raw_target,
                r.kind,
            );
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
            *ref_kinds.entry(r.kind.name()).or_default() += 1;
        }
        // A URI record and its reference are paired by span, so a record with
        // no reference would be a silently dropped import.
        assert_eq!(
            facts.header.uris.len(),
            facts.refs.len(),
            "{rel}: URI records and references disagree",
        );
        for spec in &facts.header.uris {
            combinators += spec.combinators.len() as u64;
            match &spec.form {
                UriForm::Literal(uri) => *schemes.entry(scheme_of(uri)).or_default() += 1,
                UriForm::Dynamic => dynamic += 1,
            }
        }
        // Every file declares the library an `import` names, first, whether or
        // not it declares anything else.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no library",
        );
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
        }
    }
    println!("             kinds {ref_kinds:?} dynamic {dynamic}");
    println!("             uris  {schemes:?} combinator-names {combinators}");
    println!("             defs  {kinds:?}");

    // -- the definitions, exactly ------------------------------------------

    let want: BTreeMap<u8, u64> = DEFS.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        kinds, want,
        "the definition census moved; tier 2's own deliverable is half \
         definitions and no rate can see them",
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
    assert_eq!(ref_kinds.get("import").copied(), Some(IMPORTS));
    assert_eq!(ref_kinds.get("export").copied(), Some(EXPORTS));
    // Dart forbids an interpolated URI, and this corpus writes none.
    assert_eq!(dynamic, 0);
    assert_eq!(combinators, COMBINATOR_NAMES);

    // -- the shape of the denominator -------------------------------------

    let want: BTreeMap<&str, u64> = SCHEMES.iter().copied().collect();
    assert_eq!(
        schemes, want,
        "the URI census moved; which references sit outside the rate is the \
         one thing a high rate can hide",
    );
    // The 75 inside the rate and the 49 outside it, derived from the census
    // rather than restated, so the two cannot drift apart.
    let inside = schemes["own-package"] + schemes["relative"];
    let outside = schemes["sdk"] + schemes["declared-package"];
    assert_eq!(measured.resolved + measured.unresolved, inside);
    assert_eq!(measured.external, outside);

    // Every relative URI and every `package:collection` URI names a file that
    // is really in this snapshot, so nothing misses. Zero is a measurement:
    // the denominator is 75 and every one of them linked.
    assert_eq!(measured.resolved, 75);
    // 29 `dart:` URIs and 20 `package:test/…`, and not one more: a `package:`
    // URI naming this repository's own package is resolved in `lib/` and can
    // miss, and is never waved through as external.
    assert_eq!(measured.external, 49);
    // Tier 2 emits no expression-level reference, so nothing can name a local.
    // The bucket that sits outside both rate terms is empty, which is what
    // makes this rate un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    assert_eq!(measured.unresolved, 0);
    assert!(
        reasons.is_empty(),
        "an unexpected reason appeared: {reasons:?}"
    );

    // -- the definitions the store kept, by kind and by name ---------------

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let mut packages = 0u64;
    let mut externals = 0u64;
    read.for_each_node(|_, record| {
        match record {
            NodeRecord::Definition { kind, .. } => *stored.entry(kind).or_default() += 1,
            NodeRecord::Package { .. } => packages += 1,
            NodeRecord::External { .. } => externals += 1,
        }
        Ok(())
    })
    .expect("walking the node table");
    println!("             nodes {stored:?} packages {packages} externals {externals}");
    let want: BTreeMap<u8, u64> = STORED.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "the stored definition census moved");
    assert_eq!(packages, PACKAGES, "the stored package census moved");
    assert_eq!(externals, EXTERNALS, "the stored external census moved");
    // One library per file and no more: two files sharing a library identity
    // would be the FQN grammar's root failing.
    assert_eq!(packages as usize, owned.len());
    // Nothing in this corpus declares one name twice in one scope.
    assert_eq!(report.fqn_collisions, 0);

    for (fqn, kind, file, line) in PINNED {
        // An external node's identity carries the `external:` prefix the
        // driver mints it under; a definition's is its FQN as written here.
        let spelled = match kind {
            NodeKind::External => format!("external:{fqn}"),
            _ => (*fqn).to_string(),
        };
        let id = node_id(Domain::Dart, &spelled);
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

    for (fqn, file, lines) in MERGED {
        let id = node_id(Domain::Dart, fqn);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("{fqn} is not in the store"));
        let here: Vec<u32> = def
            .declarations
            .iter()
            .filter(|d| d.file == *file)
            .map(|d| d.line)
            .collect();
        assert_eq!(
            here, *lines,
            "{fqn} lost a half: two declarations of one member are one node \
             carrying both sites, not one site or two nodes",
        );
    }

    // -- the edge a wrong answer would have been just as happy to make -----

    // `lib/equality.dart` is a barrel and `lib/src/equality.dart` is what it
    // re-exports. Three files under `lib/src/` write `import 'equality.dart'`,
    // and a resolver that anchored a relative URI anywhere but at the
    // referring file would bind all three to the barrel — a confident, wrong
    // edge rather than a miss.
    let inner = node_id(Domain::Dart, &library_fqn("lib/src/equality.dart"));
    let barrel = node_id(Domain::Dart, &library_fqn("lib/equality.dart"));
    let to_inner: Vec<(String, u32)> = references(&read, &inner)
        .expect("references")
        .into_iter()
        .map(|s| (s.file, s.line))
        .collect();
    assert_eq!(
        to_inner,
        vec![
            // The two barrels that re-export it, and the three siblings that
            // import it by relative path.
            ("lib/collection.dart".to_string(), 13),
            ("lib/equality.dart".to_string(), 9),
            ("lib/src/equality_map.dart".to_string(), 7),
            ("lib/src/equality_set.dart".to_string(), 7),
            ("lib/src/list_extensions.dart".to_string(), 11),
        ],
    );
    assert!(
        references(&read, &barrel).expect("references").is_empty(),
        "the deprecated barrel gained an incoming reference; a relative URI \
         was anchored somewhere other than the referring file",
    );
    drop(read);

    // -- the ratchet ------------------------------------------------------

    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Dart.name(),
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
