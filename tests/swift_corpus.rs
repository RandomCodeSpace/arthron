//! Acceptance for the Swift track against the alamofire corpus: nothing is
//! dropped, and the measured counts are the ones the committed baseline was
//! recorded from.
//!
//! Three questions; the first and the last are the two every tier-1 corpus
//! test asks, and the middle one is the half of tier 2 no rate reaches — and
//! for Swift it is not the smaller half, it is nearly all of it:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** 170 references and 3,712 definitions: alamofire's
//!    import surface is a rounding error beside its structure, and the rate
//!    can only see the imports. So the definition census is asserted exactly
//!    on both sides of the store, plus the 194 extensions that produce no node
//!    at all — an owner-frame bug that lost most of the corpus's methods moves
//!    no rate, no bucket and no baseline, so nothing else here would notice
//!    it.
//! 3. **The ratchet.** The counts are compared against
//!    `baselines/swift-alamofire.toml` through the same
//!    [`arthron::gate::evaluate`] the `arthron gate` command uses, so a rate
//!    regression — or drift in either of the two buckets that sit outside the
//!    rate — fails the build.
//!
//! **Read the rate with its shape in view.** Swift's is 100% over 40
//! references, and that is not a claim that Swift resolution is solved: 130 of
//! the corpus's 170 imports name the platform and sit outside both rate terms,
//! and the 43 files of `Source/` see each other through no reference text at
//! all, so there is nothing there for a rate to be taken over. The number that
//! moves when this track breaks is the census below it. See
//! `src/track_swift.rs` for the long form.
//!
//! Beside the ratchet sits the tally itself, restated. alamofire is pinned and
//! is never edited, so every number below is a fact about this extractor and
//! this resolver reading a fixed 91 files; a change to any of them is a change
//! in what the track *does*, and must arrive as a deliberate edit here and a
//! deliberate `--rebase` beside it, never as a test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/swift/alamofire --language swift \
//!     --baseline baselines/swift-alamofire.toml --rebase --commit <sha>
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken track.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefFacets, DefKind, Domain, Lang, RefKind, node_id, reason_name};
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_swift::extract::extract;
use arthron::track_swift::resolve::scan_swift;

mod support;

const CORPUS: &str = "corpus/swift/alamofire";
const BASELINE: &str = "baselines/swift-alamofire.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
const FILES: usize = 91;
const REFERENCES: u64 = 170;

/// `@testable import Alamofire`, counted.
///
/// A facet of the measurement and never a resolution rule: `@testable` widens
/// the imported module's `internal` declarations into scope, which changes
/// which members a name reaches and changes nothing about which module is
/// named — and naming the module is all this tier resolves.
///
/// Fourteen, not the thirteen a line-oriented count of the corpus finds:
/// `Tests/ProtectedTests.swift` writes the attribute on its own line, and the
/// parse decides rather than the line break.
const TESTABLE: u64 = 14;

/// `extension` declarations. **The number this track exists to get right.**
///
/// An extension declares members without declaring a node of its own, so
/// nothing in the definition census can be counted to find one. 194 of them
/// over 91 files is what "member lookup cannot be answered from the type's own
/// file" looks like from the outside, and it is the single largest structural
/// fact about this corpus.
const EXTENSIONS: u64 = 194;

/// Every module named by an import, and how often. Asserted whole.
///
/// Fifteen modules, of which exactly one — `Alamofire` — is a target this
/// package builds. That single row is the whole of the corpus's in-repository
/// import surface, and the other fourteen are the platform. Pinning the map
/// rather than the totals is what makes an in-repository target laundered into
/// `External` visible: it would arrive here as a new external name, not as a
/// number that moved by one.
const MODULES: &[(&str, u64)] = &[
    ("Alamofire", 40),
    ("Combine", 2),
    ("CoreServices", 1),
    ("Dispatch", 5),
    ("Foundation", 71),
    ("FoundationNetworking", 1),
    ("MobileCoreServices", 1),
    ("Network", 1),
    ("PackageDescription", 4),
    ("Security", 4),
    ("SystemConfiguration", 2),
    ("Testing", 5),
    ("UniformTypeIdentifiers", 1),
    ("XCTest", 31),
    ("zlib", 1),
];

/// Every definition the extractor emits over those 91 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. Definitions
/// are the half of tier 2 the import-rate gate cannot see, and here they
/// outnumber the references twenty to one. `Module` counts the 91 module
/// placeholders — one per file, each carrying no name, because no Swift file
/// states which module it belongs to.
///
/// `DefKind::Function` is absent, and that is a fact about Swift rather than a
/// gap: alamofire declares no free function at all. Every `func` in the corpus
/// sits in a type, a protocol or an extension.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Method, 1859),
    (DefKind::Type, 383),
    (DefKind::Const, 249),
    (DefKind::Constructor, 127),
    (DefKind::Field, 615),
    (DefKind::Property, 349),
    (DefKind::Module, 91),
    (DefKind::Alias, 39),
];

/// Definition nodes the store holds, by kind.
///
/// Lower than [`DEFS`] where two declarations share an identity: the corpus has
/// 74 `#if` blocks and both arms are read as written, so a member declared
/// once per platform is two declarations of one name. The pair of censuses is
/// the point — the extractor's says nothing was lost on the way in, the
/// store's says nothing was lost or over-merged on the way through.
///
/// `DefKind::Module` is absent because the driver files a module as a
/// *package* node rather than a definition; those are counted by [`PACKAGES`]
/// instead.
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Method, 1737),
    (DefKind::Type, 382),
    (DefKind::Const, 249),
    (DefKind::Constructor, 123),
    (DefKind::Field, 610),
    (DefKind::Property, 335),
    (DefKind::Alias, 39),
];

/// Package nodes: the two targets the manifest declares, plus the four
/// manifests themselves.
///
/// Six, not two. SwiftPM compiles each `Package*.swift` as a module of its own
/// and none of them is under a target's directory, so each is its own module
/// under the `$` prefix no target name can carry. Four manifests declaring
/// `let package` are four declarations, and one shared identity for them would
/// merge modules no toolchain ever builds together.
const PACKAGES: &[&str] = &[
    "$Package",
    "$Package@swift-6.0",
    "$Package@swift-6.1",
    "$Package@swift-6.2",
    "Alamofire",
    "AlamofireTests",
];

/// External nodes: every module named by an import that this package does not
/// build, spelled out.
///
/// Named rather than counted, because which modules are outside the package is
/// a claim about the world and `External` sits outside **both** terms of the
/// resolution rate. A target this build ever stopped seeing would appear in
/// this list, and the list is what would fail.
const EXTERNALS: &[&str] = &[
    "Combine",
    "CoreServices",
    "Dispatch",
    "Foundation",
    "FoundationNetworking",
    "MobileCoreServices",
    "Network",
    "PackageDescription",
    "Security",
    "SystemConfiguration",
    "Testing",
    "UniformTypeIdentifiers",
    "XCTest",
    "zlib",
];

/// Distinct FQNs a definition in more than one file claims. Data, never a
/// gate — and here every one of the six is a known limit of the FQN grammar
/// rather than a bug:
///
/// - `Alamofire.AlamofireExtension.` + backtick-`default`: two *constrained*
///   extensions — `where ExtendedType: URLSessionConfiguration` and `where
///   ExtendedType == SecPolicy` — declare a static of one name. The `where`
///   clause is not part of the identity here.
/// - the five `AlamofireTests.TestCertificates` rows: `private enum
///   TestCertificates` written in two test files. A `private` declaration at
///   file scope is file-scoped in Swift, and the identity does not carry the
///   file.
const COLLISIONS: u64 = 6;

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. Three `Session.request`
/// overloads cannot all be right unless argument labels are in the identity;
/// `Alamofire.URLRequest.method` cannot be right unless an extension files its
/// members under the type it extends *and* declares no type of its own; and
/// `AlamofireTests.BaseTestCase.assert(on:assertions:)` cannot be right unless
/// the manifest decided which of two modules a file is in.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // The module 43 files declare and none of them names.
    ("Alamofire", NodeKind::Package, "Source/Alamofire.swift", 1),
    (
        "AlamofireTests",
        NodeKind::Package,
        "Tests/BaseTestCase.swift",
        1,
    ),
    // A manifest is its own module, and its own declaration is in it.
    ("$Package", NodeKind::Package, "Package.swift", 1),
    (
        "$Package.package",
        NodeKind::Definition(DefKind::Const),
        "Package.swift",
        28,
    ),
    (
        "Alamofire.Session",
        NodeKind::Definition(DefKind::Type),
        "Source/Core/Session.swift",
        30,
    ),
    // Three overloads of one base name, told apart the way Swift tells them
    // apart. One node for all three would be a census that under-counts the
    // API surface it claims to measure.
    (
        "Alamofire.Session.request(_:method:parameters:encoding:headers:interceptor:shouldAutomaticallyResume:requestModifier:)",
        NodeKind::Definition(DefKind::Method),
        "Source/Core/Session.swift",
        318,
    ),
    (
        "Alamofire.Session.request(_:method:parameters:encoder:headers:interceptor:shouldAutomaticallyResume:requestModifier:)",
        NodeKind::Definition(DefKind::Method),
        "Source/Core/Session.swift",
        367,
    ),
    (
        "Alamofire.Session.request(_:interceptor:shouldAutomaticallyResume:)",
        NodeKind::Definition(DefKind::Method),
        "Source/Core/Session.swift",
        393,
    ),
    // Members an extension declares on a type Foundation owns: the members are
    // this repository's and the type is not, and the identity says both.
    (
        "Alamofire.URLRequest.method",
        NodeKind::Definition(DefKind::Property),
        "Source/Extensions/URLRequest+Alamofire.swift",
        29,
    ),
    (
        "Alamofire.URLRequest.validate()",
        NodeKind::Definition(DefKind::Method),
        "Source/Extensions/URLRequest+Alamofire.swift",
        34,
    ),
    (
        "Alamofire.AFError",
        NodeKind::Definition(DefKind::Type),
        "Source/Core/AFError.swift",
        33,
    ),
    (
        "Alamofire.AFError.sessionInvalidated",
        NodeKind::Definition(DefKind::Const),
        "Source/Core/AFError.swift",
        225,
    ),
    (
        "Alamofire.HTTPMethod.get",
        NodeKind::Definition(DefKind::Field),
        "Source/Core/HTTPMethod.swift",
        35,
    ),
    // A test-module declaration: the manifest, and nothing in the file, is
    // what puts it under `AlamofireTests` rather than `Alamofire`.
    (
        "AlamofireTests.BaseTestCase.assert(on:assertions:)",
        NodeKind::Definition(DefKind::Method),
        "Tests/BaseTestCase.swift",
        119,
    ),
    // An extension head that is not an identifier becomes the owner segment
    // it is written as — a recorded limit of the FQN grammar, pinned here so
    // it stays a limit somebody chose rather than one somebody rediscovers.
    // Four of the corpus's 194 extensions are this shape.
    (
        "Alamofire.[HTTPHeader].index(of:)",
        NodeKind::Definition(DefKind::Method),
        "Source/Core/HTTPHeaders.swift",
        338,
    ),
    (
        "Alamofire.Collection<String>.qualityEncoded()",
        NodeKind::Definition(DefKind::Method),
        "Source/Core/HTTPHeaders.swift",
        436,
    ),
    ("XCTest", NodeKind::External, "Tests/BaseTestCase.swift", 27),
    (
        "Foundation",
        NodeKind::External,
        "Source/Core/Session.swift",
        25,
    ),
];

/// A node that must **not** exist: `extension URLRequest` declares members, not
/// a type, and a node here would put Foundation's type in this repository's own
/// definition table.
const ABSENT: &[&str] = &["Alamofire.URLRequest", "Alamofire.URLSession", "URLRequest"];

#[test]
fn the_swift_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        support::missing(corpus);
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_swift(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Swift.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "swift        resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
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
    let mut modules: BTreeMap<String, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    let mut testable = 0u64;
    let mut extensions = 0u64;
    let mut static_types = 0u64;
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
        // An import clause and its reference are paired by span, so a clause
        // with no reference would be a silently dropped import.
        assert_eq!(
            facts.header.imports.len(),
            facts.refs.len(),
            "{rel}: import clauses and import references disagree",
        );
        for spec in &facts.header.imports {
            *modules.entry(spec.path[0].clone()).or_default() += 1;
            if spec.testable {
                testable += 1;
            }
        }
        extensions += facts.header.extensions.len() as u64;
        // Every file declares the module it is in — without naming it, which
        // is the whole of Swift's resolution problem in one record.
        let placeholder = facts.defs.first().expect("a module placeholder");
        assert_eq!(placeholder.kind, DefKind::Module, "{rel}");
        assert_eq!(placeholder.name, "", "{rel} named its own module");
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
            if d.kind == DefKind::Type && d.facets.contains(DefFacets::STATIC) {
                static_types += 1;
            }
        }
    }
    println!("             imports {modules:?}");
    println!("             testable {testable} extensions {extensions}");
    println!("             defs  {kinds:?}");

    // -- the definitions, exactly ------------------------------------------

    let want: BTreeMap<u8, u64> = DEFS.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        kinds, want,
        "the definition census moved; tier 2's own deliverable is mostly \
         definitions and no rate can see them",
    );
    assert_eq!(
        extensions, EXTENSIONS,
        "the extension census moved; an extension declares no node of its own, \
         so nothing else here can see one",
    );
    assert_eq!(testable, TESTABLE);
    // Swift has no `static` type declaration to write, so no type node may
    // carry the facet. It is checked here because the census above counts by
    // kind and cannot see a facet: `class Foo {}` carries the same `class`
    // child that makes `class func` static, and reading the one as the other
    // put a false fact on 174 of these 383 nodes with every test still green.
    // Facets are stored and are part of a node's payload, so this is graph
    // data rather than a display detail.
    assert_eq!(
        static_types, 0,
        "a type declaration carries STATIC; Swift has no way to write one",
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
    let want: BTreeMap<String, u64> = MODULES
        .iter()
        .map(|(m, n)| ((*m).to_string(), *n))
        .collect();
    assert_eq!(
        modules, want,
        "the import surface moved; exactly one of these modules is a target \
         this package builds and the rest are the platform",
    );

    // Every `import Alamofire` in `Tests/`, and nothing else: the 43 files of
    // `Source/` are Alamofire and none of them imports it.
    assert_eq!(measured.resolved, 40);
    // The platform, and the package manager's own module. Large, and outside
    // both terms of the rate — see the module header before reading the rate.
    assert_eq!(measured.external, 130);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The bucket that sits outside both rate terms is empty, which is
    // what makes this rate un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    // Every import in the corpus is either a target this package builds or a
    // module outside it, and the manifest says which. Nothing is guessed and
    // nothing is left over.
    assert_eq!(measured.unresolved, 0);
    assert!(
        reasons.is_empty(),
        "an unexpected reason appeared: {reasons:?}"
    );
    assert_eq!(
        report.fqn_collisions, COLLISIONS,
        "the collision count moved; each of the six is a recorded limit of the \
         FQN grammar and a new one is a fact to explain",
    );

    // -- the definitions the store kept, by kind and by name ---------------

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let mut packages: Vec<String> = Vec::new();
    let mut externals: Vec<String> = Vec::new();
    read.for_each_node(|_, record| {
        match record {
            NodeRecord::Definition { kind, .. } => *stored.entry(kind).or_default() += 1,
            NodeRecord::Package { import_path, .. } => packages.push(import_path.clone()),
            NodeRecord::External { package, .. } => externals.push(package.clone()),
        }
        Ok(())
    })
    .expect("walking the node table");
    packages.sort();
    externals.sort();
    println!("             nodes {stored:?}");
    println!("             packages {packages:?}");
    println!("             externals {externals:?}");
    let want: BTreeMap<u8, u64> = STORED.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "the stored definition census moved");
    assert_eq!(packages, PACKAGES, "the stored module census moved");
    assert_eq!(
        externals, EXTERNALS,
        "the external set moved; a target read out of the manifest that stopped \
         being read would appear here and vanish from both rate terms",
    );

    for (fqn, kind, file, line) in PINNED {
        // An external node's identity carries the `external:` prefix the
        // driver mints it under; a definition's is its FQN as written here.
        let spelled = match kind {
            NodeKind::External => format!("external:{fqn}"),
            _ => (*fqn).to_string(),
        };
        let id = node_id(Domain::Swift, &spelled);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("{fqn} is not in the store"));
        assert_eq!(def.node.name, *fqn);
        assert_eq!(def.node.kind, *kind, "{fqn}");
        // A module is declared by every file of its target and an external by
        // every file that imports it, so only the sites in the file this pin
        // names are worth printing when it misses.
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

    for fqn in ABSENT {
        let id = node_id(Domain::Swift, fqn);
        assert!(
            definition(&read, &id).expect("query").is_none(),
            "{fqn} is in the store; an extension declares members, not the type it extends",
        );
    }
    drop(read);

    // -- the ratchet ------------------------------------------------------

    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Swift.name(),
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
