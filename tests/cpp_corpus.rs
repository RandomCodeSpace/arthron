//! Acceptance for the C++ track against the fmt corpus: nothing is dropped,
//! and the measured counts are the ones the committed baseline was recorded
//! from.
//!
//! Four questions. The first and the last are the two every corpus test here
//! asks; the middle two are the halves no rate reaches.
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** Tier 2's deliverable is definitions, structure and
//!    imports, and the rate can only see the imports. The definition census
//!    is therefore asserted exactly on both sides of the store — the Ruby
//!    review found an owner-frame bug that lost 566 of 633 methods while every
//!    rate, bucket and baseline stayed green, and nothing but a census
//!    notices that.
//! 3. **Where the misses are.** 114 of the 115 unresolved references are one
//!    reason, and reading that number without its shape is how a floor gets
//!    mistaken for a bug. The split by include syntax is asserted, because
//!    the one angled miss is the anti-laundering case: `<fmt/base.h>` names a
//!    file that is really in this repository, and putting it in `External`
//!    would move it outside both terms of the rate.
//! 4. **The ratchet.** The counts are compared against
//!    `baselines/cpp-fmt.toml` through the same [`arthron::gate::evaluate`]
//!    the `arthron gate` command uses, so a rate regression — or drift in
//!    either of the two buckets that sit outside the rate — fails the build.
//!
//! # Why this rate is small, stated where it is measured
//!
//! fmt is header-dominated and **its headers are all `.h`**, which is an
//! extension the tier-2 registration deliberately left unclaimed and which
//! going live did not widen. 21 of the corpus's 55 source files are `.h`; the
//! 33 this track reads name one of them in 99 of their 116 quoted `#include`
//! directives, and in one angled directive besides. Those 100 references are
//! `Unresolved` with an honest reason and count *against* the rate. The floor
//! is the extension policy's, not the resolver's, and the one change that
//! would move it is a separate, ratified decision to parse C headers under a
//! C++ grammar.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/cpp/fmt --language cpp \
//!     --baseline baselines/cpp-fmt.toml --rebase --commit 1be298e
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
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Store, StoredOutcome};
use arthron::track_cpp::extract::{IncludeForm, extract};
use arthron::track_cpp::lang::{module_fqn, unit_fqn};
use arthron::track_cpp::resolve::scan_cpp;

const CORPUS: &str = "corpus/cpp/fmt";
const BASELINE: &str = "baselines/cpp-fmt.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
///
/// 33 files and not 55: the 21 `.h` headers and the one `.c` translation unit
/// carry extensions this build does not claim, so no scan reads them.
const FILES: usize = 33;
const REFERENCES: u64 = 273;
const QUOTED: u64 = 116;
const ANGLED: u64 = 155;
const MODULE: u64 = 2;

/// Every definition the extractor emits over those 33 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. `Module`
/// counts the 33 synthetic unit nodes and the one `export module fmt;`
/// alongside the namespaces the source writes.
///
/// `Function` is 177 rather than the 774 an earlier draft of this extractor
/// produced: a macro invocation followed by a braced block is a
/// `function_definition` to this grammar, and 600 of them were googletest's
/// `TEST(suite, case) { … }`. C++ gives every function a declared return type
/// except a constructor, a destructor and a conversion function, which is the
/// rule that tells the two apart — see
/// [`arthron::track_cpp::extract`]. 24 survive it, where a
/// `FMT_END_NAMESPACE` that expands to nothing stands where a return type
/// would be; without running a preprocessor there is nothing left to tell
/// those from a function returning `FMT_END_NAMESPACE`, and this build runs
/// none.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 177),
    (DefKind::Method, 148),
    (DefKind::Type, 181),
    (DefKind::Const, 17),
    (DefKind::Var, 18),
    (DefKind::Constructor, 27),
    (DefKind::Module, 61),
    (DefKind::Alias, 37),
];

/// Definition nodes the store holds after merging, by kind.
///
/// Lower than [`DEFS`] where C++'s one-definition rule merges: a class
/// declared in one file and written again in another is one entity, and so is
/// a prototype and its body. The pair of censuses is the point — the
/// extractor's says nothing was lost on the way in, the store's says nothing
/// was lost or over-merged on the way through.
///
/// `DefKind::Module` is absent because the driver files a module as a
/// *package* node rather than a definition; those are counted by [`PACKAGES`].
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Function, 113),
    (DefKind::Method, 134),
    (DefKind::Type, 171),
    (DefKind::Const, 15),
    (DefKind::Var, 17),
    (DefKind::Constructor, 18),
    (DefKind::Alias, 35),
];

/// Package nodes: the 33 unit nodes an `#include` names, the one named module
/// an `import` names, and the namespaces the source declares once reopening
/// has merged them.
const PACKAGES: u64 = 47;

/// External nodes: one per distinct system or platform header the corpus
/// includes with angle brackets and no include root supplies. Named rather
/// than only counted in [`PINNED`], because which header is outside this
/// repository is a claim and not a tally.
const EXTERNALS: u64 = 71;

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. `@fmt` and `fmt` cannot
/// both be right unless a named module and a namespace of one name are two
/// identities, and `#src/os.cc` cannot be right unless a quoted include
/// reaching `../src/` from `test/` lands on the same node the sibling include
/// in `src/fmt.cc` lands on.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // The three units a quoted `#include` actually resolves to in this
    // corpus: `src/fmt.cc:149` includes `"format.cc"`, `:152` includes
    // `"os.cc"`, and `test/posix-mock-test.cc:20` includes `"../src/os.cc"`.
    ("#src/format.cc", NodeKind::Package, "src/format.cc", 1),
    ("#src/os.cc", NodeKind::Package, "src/os.cc", 1),
    // The C++20 module `test/module-test.cc` imports, declared by a grammar
    // that has no rule for the declaration that declares it.
    ("@fmt", NodeKind::Package, "src/fmt.cc", 101),
    // Structure from the one compiled source that is not a test.
    (
        "buffered_file::~buffered_file",
        NodeKind::Definition(DefKind::Method),
        "src/os.cc",
        170,
    ),
    (
        "buffered_file::buffered_file",
        NodeKind::Definition(DefKind::Constructor),
        "src/os.cc",
        175,
    ),
    // An out-of-line member definition. Filed under the right owner with the
    // weaker kind, because one file cannot say whether `buffered_file` is a
    // class or a namespace — the limit `track_cpp::extract` records, pinned
    // so that closing it is a deliberate change and not a silent one.
    (
        "buffered_file::close",
        NodeKind::Definition(DefKind::Function),
        "src/os.cc",
        183,
    ),
    (
        "format_facet::int_formatter",
        NodeKind::Definition(DefKind::Type),
        "test/format-test.cc",
        2399,
    ),
    // A system header, outside this repository and reached only through the
    // angled syntax.
    ("vector", NodeKind::External, "src/fmt.cc", 55),
];

#[test]
fn the_cpp_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_cpp(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Cpp.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "cpp          resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
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
    for rel in &owned {
        assert!(
            !rel.ends_with(".h") && !rel.ends_with(".c"),
            "{rel}: this build claims neither extension, and going live widened nothing",
        );
    }

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
        // A clause and its reference are paired by span, so a clause with no
        // reference would be a silently dropped include.
        assert_eq!(
            facts.header.includes.len(),
            facts.refs.len(),
            "{rel}: include clauses and import references disagree",
        );
        for spec in &facts.header.includes {
            *forms
                .entry(match spec.form {
                    IncludeForm::Quoted(_) => "quoted",
                    IncludeForm::Angle(_) => "angled",
                    IncludeForm::Module(_) => "module",
                    IncludeForm::Computed => "computed",
                })
                .or_default() += 1;
        }
        // Every file declares the unit an `#include` names, first, whether or
        // not it declares anything else.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no unit",
        );
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
        }
    }
    println!("             forms {forms:?}");
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
    assert_eq!(forms.get("quoted").copied(), Some(QUOTED));
    assert_eq!(forms.get("angled").copied(), Some(ANGLED));
    assert_eq!(forms.get("module").copied(), Some(MODULE));
    // fmt spells no `#include` with a macro. A shape the corpus does not
    // exercise is recorded as absent rather than assumed.
    assert_eq!(forms.get("computed").copied(), None);

    // Three quoted includes name a `.cc` file — the only in-repository
    // translation units this build parses — and `import fmt;` names the one
    // module `src/fmt.cc` exports.
    assert_eq!(measured.resolved, 4);
    // Every angled include but one names a header no include root supplies.
    assert_eq!(measured.external, 154);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The bucket that sits outside both rate terms is empty, which is
    // what makes this rate un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    assert_eq!(measured.unresolved, 115);

    // The floor, named. 113 quoted includes name a `.h` header or the
    // unvendored googletest bundle; one angled include names `fmt/base.h`,
    // which is a real file in this repository under an extension this build
    // does not parse.
    assert_eq!(reasons.get("ModuleNotFound").copied(), Some(114));
    // `import std;` names the standard library's module. No `export module`
    // here declares it and this build indexes no standard-library set.
    assert_eq!(reasons.get("UnknownPackage").copied(), Some(1));
    assert_eq!(
        reasons.len(),
        2,
        "an unexpected reason appeared: {reasons:?}"
    );

    // -- where the misses are, by include syntax ---------------------------

    // The load-bearing split: the single angled miss must be the
    // in-repository header, not an `External` that vanished from both terms
    // of the rate.
    let store = Store::open(&db).expect("store opens");
    let snapshot = store.snapshot().expect("snapshot");
    let mut by_syntax: BTreeMap<(&str, &str), u64> = BTreeMap::new();
    for (key, record) in &snapshot.rows {
        let syntax = match key.raw_target.chars().next() {
            Some('"') => "quoted",
            Some('<') => "angled",
            _ => "module",
        };
        let outcome = match &record.outcome {
            StoredOutcome::Resolved(_) => "resolved",
            StoredOutcome::External(_) => "external",
            StoredOutcome::Unresolved(_) => "unresolved",
        };
        *by_syntax.entry((syntax, outcome)).or_default() += u64::from(record.count);
    }
    println!("             by syntax {by_syntax:?}");
    assert_eq!(by_syntax.get(&("quoted", "resolved")).copied(), Some(3));
    assert_eq!(by_syntax.get(&("quoted", "unresolved")).copied(), Some(113));
    assert_eq!(
        by_syntax.get(&("quoted", "external")).copied(),
        None,
        "a quoted include says this project supplies the header; a miss is \
         this project's floor and is never laundered into `External`",
    );
    assert_eq!(by_syntax.get(&("angled", "external")).copied(), Some(154));
    assert_eq!(
        by_syntax.get(&("angled", "unresolved")).copied(),
        Some(1),
        "`<fmt/base.h>` is a file in this repository under an extension this \
         build does not parse; `External` would move it outside both terms of \
         the rate",
    );
    assert_eq!(by_syntax.get(&("module", "resolved")).copied(), Some(1));
    assert_eq!(by_syntax.get(&("module", "unresolved")).copied(), Some(1));
    drop(store);

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

    for (fqn, kind, file, line) in PINNED {
        // An external node's identity carries the `external:` prefix the
        // driver mints it under; a definition's is its FQN as written here.
        let spelled = match kind {
            NodeKind::External => format!("external:{fqn}"),
            _ => (*fqn).to_string(),
        };
        let id = node_id(Domain::Cxx, &spelled);
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

    // A namespace and a named module of one name are two identities. fmt
    // writes `namespace fmt` in most of the corpus and `export module fmt;`
    // once, and sharing an identity would make `import fmt;` resolve to a
    // namespace and call it an edge.
    let namespace = node_id(Domain::Cxx, "fmt");
    let module = node_id(Domain::Cxx, &module_fqn("fmt"));
    assert_ne!(namespace, module);
    assert!(
        definition(&read, &namespace)
            .expect("namespace read")
            .is_some(),
        "the namespace `fmt` is not in the store",
    );

    // The extension policy, asserted where it costs the most. Every header
    // fmt publishes is a `.h` file, and no scan read one, so no unit node
    // exists for any of them — which is precisely why 100 references are a
    // floor rather than an edge.
    for header in [
        "include/fmt/format.h",
        "include/fmt/base.h",
        "test/gtest-extra.h",
    ] {
        let id = node_id(Domain::Cxx, &unit_fqn(header));
        assert!(
            definition(&read, &id).expect("header read").is_none(),
            "{header} has a unit node; this build does not parse `.h`",
        );
    }
    drop(read);

    // -- the ratchet ------------------------------------------------------

    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Cpp.name(),
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
