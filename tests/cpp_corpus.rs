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
//!
//!    That check covers the *resolver* and structurally cannot cover the
//!    extractor: re-extracting with the same extractor agrees with itself on
//!    both sides of a drop, and every count below is pinned from the
//!    extractor's own output, so an extractor that lost a directive would
//!    enshrine the loss as ground truth. One did — a `#if` condition the
//!    pinned grammar cannot parse used to swallow every `#include` after it —
//!    so the directive census is taken a second time by
//!    [`directives`], which reads the file as preprocessing lines and shares
//!    no code, no grammar and no assumption with the extractor. Whichever of
//!    the two is wrong, they disagree.
//! 2. **The definitions.** Tier 2's deliverable is definitions, structure and
//!    imports, and the rate can only see the imports. The definition census
//!    is therefore asserted exactly on both sides of the store — the Ruby
//!    review found an owner-frame bug that lost 566 of 633 methods while every
//!    rate, bucket and baseline stayed green, and nothing but a census
//!    notices that.
//! 3. **Where the misses are.** 17 of the 18 unresolved references are one
//!    reason — quoted includes naming the googletest bundle the corpus
//!    deliberately does not vendor — and reading that number without its
//!    shape is how a floor gets mistaken for a bug. The split by include
//!    syntax is asserted, and so is the one angled *hit*: `<fmt/base.h>`
//!    names a file that is really in this repository, and now that `.h` is
//!    read it must be `Resolved` — before the claim it was the one angled
//!    miss, held out of `External` so it could not vanish from both terms
//!    of the rate. The FQN-collision count is asserted beside them: C++ is
//!    the only live track that reports a non-zero one, and an unpinned
//!    number this track alone emits is a number that drifts.
//! 4. **The ratchet.** The counts are compared against
//!    `baselines/cpp-fmt.toml` through the same [`arthron::gate::evaluate`]
//!    the `arthron gate` command uses, so a rate regression — or drift in
//!    either of the two buckets that sit outside the rate — fails the build.
//!
//! # What the rate measures now, stated where it is measured
//!
//! fmt is header-dominated and **its headers are all `.h`** — 21 of the
//! corpus's 55 source files. Under the six-extension registration no scan
//! read one, 100 in-repository header references were an extension-policy
//! floor, and this rate was 3.4%. The `.h` claim — the amendment the
//! registration reserved for the commit that parses the extension — makes
//! the corpus measurable: 54 files read, and the floor that remains is the
//! googletest bundle the corpus deliberately does not vendor (17 quoted
//! includes) plus the one `import std;` no repository can supply. The rate
//! now measures the resolver, not the policy.
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

mod support;

const CORPUS: &str = "corpus/cpp/fmt";
const BASELINE: &str = "baselines/cpp-fmt.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
///
/// 54 files and not 55: the 21 `.h` headers are read now that `.h` is
/// claimed, and the one `.c` translation unit still carries an extension
/// this build does not.
const FILES: usize = 54;
const REFERENCES: u64 = 399;
const QUOTED: u64 = 142;
const ANGLED: u64 = 255;
const MODULE: u64 = 2;

/// Every definition the extractor emits over those 54 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. `Module`
/// counts the 54 synthetic unit nodes and the one `export module fmt;`
/// alongside the namespaces the source writes.
///
/// `Function` excludes googletest's `TEST(suite, case) { … }` blocks: a
/// macro invocation followed by a braced block is a `function_definition`
/// to this grammar, and the first census — taken when the six-extension
/// world read 33 files — found 600 of them. C++ gives every function a
/// declared return type except a constructor, a destructor and a
/// conversion function, which is the rule that tells the two apart — see
/// [`arthron::track_cpp::extract`]. What survives it is the handful where
/// a macro such as `FMT_END_NAMESPACE` that expands to nothing stands
/// where a return type would be; without running a preprocessor there is
/// nothing left to tell those from a function returning
/// `FMT_END_NAMESPACE`, and this build runs none.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 669),
    (DefKind::Method, 795),
    (DefKind::Type, 457),
    (DefKind::Const, 101),
    (DefKind::Var, 44),
    (DefKind::Constructor, 121),
    (DefKind::Module, 110),
    (DefKind::Alias, 206),
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
    (DefKind::Function, 388),
    (DefKind::Method, 694),
    (DefKind::Type, 427),
    (DefKind::Const, 99),
    (DefKind::Var, 34),
    (DefKind::Constructor, 70),
    (DefKind::Alias, 194),
];

/// Package nodes: the 54 unit nodes an `#include` names, the one named module
/// an `import` names, and the namespaces the source declares once reopening
/// has merged them.
const PACKAGES: u64 = 75;

/// External nodes: one per distinct system or platform header the corpus
/// includes with angle brackets and no include root supplies. Named rather
/// than only counted in [`PINNED`], because which header is outside this
/// repository is a claim and not a tally.
const EXTERNALS: u64 = 83;

/// Definition nodes more than one file declares — what `arthron scan` prints
/// as `fqn collisions`.
///
/// Every live track prints it; C++ is the only one whose count is not zero,
/// which makes it the only one where leaving it unasserted lets it drift.
/// All 90 are ordinary C++ across translation units rather than an identity
/// bug, and the inventory splits 44/29/17: an entity declared in a header
/// and written again in a source file (the posix-mock `test::` family,
/// `output_redirect`'s members, `buffered_file`'s — the one-definition
/// rule, working exactly as `CppResolver::mergeable` intends), fmt's
/// public API written per header (`format`, `vformat`, `print`, `join`
/// across `base.h`, `format.h`, `xchar.h`, `color.h`), and the source-only
/// set the six-extension world already counted — `main` in three test
/// binaries, `operator<<` and `format_as` written per test file,
/// googletest's `TEST`. What is not a repeated entity is names shared
/// across units, which is exactly what the counter is for.
///
/// Counted here off the stored nodes rather than read from the scan's own
/// `Report`, which subtracts this event's merges and so answers 16 for a
/// cold scan of the same tree. The graph-derived number is the one a reader
/// of `arthron scan` sees, and it is the one that must not drift.
const COLLISIONS: u64 = 90;

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. `@fmt` and `fmt` cannot
/// both be right unless a named module and a namespace of one name are two
/// identities, and `#src/os.cc` cannot be right unless a quoted include
/// reaching `../src/` from `test/` lands on the same node the sibling include
/// in `src/fmt.cc` lands on.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // Units a quoted `#include` resolves to: `src/fmt.cc:149` includes
    // `"format.cc"`, `:152` includes `"os.cc"`, and
    // `test/posix-mock-test.cc:20` includes `"../src/os.cc"`.
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
    // An out-of-line member definition. In the six-extension world one file
    // could not say whether `buffered_file` was a class or a namespace, and
    // this node carried the weaker `Function` — the limit `track_cpp::
    // extract` records. With `include/fmt/os.h` read, the class body at
    // `os.h:165` says *member*, and the node carries the header's kind.
    (
        "buffered_file::close",
        NodeKind::Definition(DefKind::Method),
        "src/os.cc",
        183,
    ),
    (
        "format_facet::int_formatter",
        NodeKind::Definition(DefKind::Type),
        "test/format-test.cc",
        2399,
    ),
    // Structure from the headers the `.h` claim made readable.
    //
    // `uint128` is spelled inside `namespace detail` (`format.h:190`), and
    // is stored at the top level: under the pinned grammar the enclosing
    // frame does not survive the preprocessor-conditional region above the
    // class, where `base.h`'s `detail::` names keep theirs. The
    // no-preprocessor limit, pinned so that closing it is a deliberate
    // change and not a silent one.
    (
        "uint128",
        NodeKind::Definition(DefKind::Type),
        "include/fmt/format.h",
        293,
    ),
    // An in-class member carries the kind the class body states.
    (
        "uint128::high",
        NodeKind::Definition(DefKind::Method),
        "include/fmt/format.h",
        301,
    ),
    // A test header, because the claim covers `test/` as much as
    // `include/`.
    (
        "output_redirect",
        NodeKind::Definition(DefKind::Type),
        "test/gtest-extra.h",
        74,
    ),
    // A system header, outside this repository and reached only through the
    // angled syntax.
    ("vector", NodeKind::External, "src/fmt.cc", 55),
    // The two directives the pinned grammar used to delete. Both sit under a
    // `#if … && FMT_HAS_INCLUDE(<…>)`, whose condition ran the parse off the
    // end of the file; `<version>` also appears at `src/fmt.cc:66`, so only
    // `<ranges>` shows up as a node that was missing outright, and pinning
    // the *sites* is what makes the other one visible too.
    ("version", NodeKind::External, "test/format-test.cc", 29),
    ("ranges", NodeKind::External, "test/ranges-test.cc", 20),
];

/// Every import directive in a file, counted as `(quoted, angled, computed,
/// module)` without parsing C++.
///
/// The second opinion the completeness check needs. An `#include` is a
/// *preprocessing* line — a `#`, the directive name, and a specifier — and
/// reading it needs no grammar, which is exactly why this can disagree with
/// an extractor that has one. Deliberately naive about comments: the corpus
/// holds no commented-out directive (a comment-aware count over the same 54
/// files returns the same 399), and if one ever appears the honest response
/// is to teach *this* function about it rather than to loosen the assertion
/// it feeds.
fn directives(source: &str) -> (u64, u64, u64, u64) {
    let (mut quoted, mut angled, mut computed, mut module) = (0, 0, 0, 0);
    for line in source.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('#') {
            let rest = rest.trim_start();
            let Some(spec) = rest.strip_prefix("include") else {
                continue;
            };
            match spec.trim_start().chars().next() {
                Some('"') => quoted += 1,
                Some('<') => angled += 1,
                _ => computed += 1,
            }
            continue;
        }
        // `import fmt;` names a module; `export module fmt;` *declares* one
        // and is a definition, and `module;` names nothing at all.
        let named = line
            .strip_prefix("import ")
            .or_else(|| line.strip_prefix("module "));
        if let Some(name) = named.and_then(|n| n.strip_suffix(';'))
            && !name.trim().is_empty()
            && name
                .trim()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        {
            module += 1;
        }
    }
    (quoted, angled, computed, module)
}

#[test]
fn the_cpp_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        support::missing(corpus);
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
    println!(
        "             fqn collisions {} (this event's; the graph's is asserted below)",
        report.fqn_collisions,
    );

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
            !rel.ends_with(".c"),
            "{rel}: `.c` is unclaimed, and the `.h` amendment widened nothing else",
        );
    }
    assert!(
        owned.iter().any(|rel| rel.ends_with(".h")),
        "`.h` is claimed, and a scan of a header-dominated corpus read none",
    );

    let mut re_extracted = 0u64;
    let mut forms: BTreeMap<&str, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    let mut lexed: BTreeMap<&str, u64> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        let facts = extract(rel, &source);
        re_extracted += facts.refs.len() as u64;

        // The second opinion, per file rather than in total: a whole-corpus
        // tally can be right while two files are wrong in opposite
        // directions, and the failure this guards against deletes every
        // directive in *one* file.
        let (quoted, angled, computed, module) = directives(&source);
        *lexed.entry("quoted").or_default() += quoted;
        *lexed.entry("angled").or_default() += angled;
        *lexed.entry("computed").or_default() += computed;
        *lexed.entry("module").or_default() += module;
        assert_eq!(
            facts.refs.len() as u64,
            quoted + angled + computed + module,
            "{rel}: the extractor and a plain reading of the preprocessing \
             lines disagree about how many directives this file has",
        );
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
    println!("             lexed {lexed:?}");
    println!("             defs  {kinds:?}");

    // Form by form, not only in total: a directive miscounted as another
    // form would cancel out in a sum.
    for form in ["quoted", "angled", "computed", "module"] {
        assert_eq!(
            forms.get(form).copied().unwrap_or_default(),
            lexed.get(form).copied().unwrap_or_default(),
            "{form}: the extractor and a plain reading of the preprocessing \
             lines disagree",
        );
    }

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

    // 125 quoted includes and one angled `<fmt/base.h>` land on units this
    // scan read — most of them the `.h` headers the claim made visible —
    // and `import fmt;` names the one module `src/fmt.cc` exports.
    assert_eq!(measured.resolved, 127);
    // Every angled include but `<fmt/base.h>` names a header no include
    // root supplies.
    assert_eq!(measured.external, 254);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The bucket that sits outside both rate terms is empty, which is
    // what makes this rate un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    assert_eq!(measured.unresolved, 18);

    // The floor, named. All 17 are quoted includes naming the
    // `"gtest/gtest.h"` / `"gmock/gmock.h"` bundle the corpus deliberately
    // does not vendor; the `.h` headers that used to dominate this bucket
    // resolve now.
    assert_eq!(reasons.get("ModuleNotFound").copied(), Some(17));
    // `import std;` names the standard library's module. No `export module`
    // here declares it and this build indexes no standard-library set.
    assert_eq!(reasons.get("UnknownPackage").copied(), Some(1));
    assert_eq!(
        reasons.len(),
        2,
        "an unexpected reason appeared: {reasons:?}"
    );

    // -- where the misses are, by include syntax ---------------------------

    // The load-bearing split: the one angled include naming a file this
    // repository supplies must be the resolved one, and no quoted miss may
    // be laundered into `External`.
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
    assert_eq!(by_syntax.get(&("quoted", "resolved")).copied(), Some(125));
    assert_eq!(by_syntax.get(&("quoted", "unresolved")).copied(), Some(17));
    assert_eq!(
        by_syntax.get(&("quoted", "external")).copied(),
        None,
        "a quoted include says this project supplies the header; a miss is \
         this project's floor and is never laundered into `External`",
    );
    assert_eq!(by_syntax.get(&("angled", "external")).copied(), Some(254));
    assert_eq!(
        by_syntax.get(&("angled", "resolved")).copied(),
        Some(1),
        "`<fmt/base.h>` is a file in this repository, and now that `.h` is \
         read it must be an edge",
    );
    assert_eq!(
        by_syntax.get(&("angled", "unresolved")).copied(),
        None,
        "every angled include either lands on a unit this scan read or is \
         supplied outside this repository",
    );
    assert_eq!(by_syntax.get(&("module", "resolved")).copied(), Some(1));
    assert_eq!(by_syntax.get(&("module", "unresolved")).copied(), Some(1));
    drop(store);

    // -- the definitions the store kept, by kind and by name ---------------

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let mut packages = 0u64;
    let mut externals = 0u64;
    let mut collisions = 0u64;
    read.for_each_node(|_, record| {
        match &record {
            NodeRecord::Definition {
                kind, declarations, ..
            } => {
                *stored.entry(*kind).or_default() += 1;
                let mut files = declarations.iter().map(|d| d.file.as_str());
                if let Some(first) = files.next()
                    && files.any(|f| f != first)
                {
                    collisions += 1;
                }
            }
            NodeRecord::Package { .. } => packages += 1,
            NodeRecord::External { .. } => externals += 1,
        }
        Ok(())
    })
    .expect("walking the node table");
    println!(
        "             nodes {stored:?} packages {packages} externals {externals} \
         collisions {collisions}"
    );
    let want: BTreeMap<u8, u64> = STORED.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "the stored definition census moved");
    assert_eq!(packages, PACKAGES, "the stored package census moved");
    assert_eq!(externals, EXTERNALS, "the stored external census moved");
    assert_eq!(
        collisions, COLLISIONS,
        "the FQN-collision count moved; this is the one live track that emits \
         a non-zero one, and it is data about C++ rather than a gate",
    );

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

    // The `.h` claim, asserted where it pays the most. Every header fmt
    // publishes is a `.h` file, and the scan reads each one now, so the
    // unit node an `#include` probes for exists — which is precisely why
    // the 100 header references that used to be a floor are edges.
    for header in [
        "include/fmt/format.h",
        "include/fmt/base.h",
        "test/gtest-extra.h",
    ] {
        let id = node_id(Domain::Cxx, &unit_fqn(header));
        assert!(
            definition(&read, &id).expect("header read").is_some(),
            "{header} has no unit node; the `.h` claim is not being read",
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
