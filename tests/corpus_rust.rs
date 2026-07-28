//! Acceptance for the Rust track against the ripgrep corpus: nothing is
//! dropped, the tier-2 contract holds on real code, and the measured counts
//! hold the committed baseline.
//!
//! Rust is a **tier-2** language, so what this file gates is an
//! **import-resolution rate** — `Resolved / (Resolved + Unresolved)` over the
//! `use`, `mod` and `extern crate` references the extractor emits, and
//! nothing else. It is not comparable with Go's or Java's rate, and it is
//! never aggregated with either.
//!
//! Three questions, because a rate is only worth reading if you can answer
//! all three:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The tier.** Every stored row is an import reference, and no reference
//!    is a local binding. A tier-2 track that started emitting call sites
//!    would report tier-1 coverage nobody measured, and this is what notices.
//! 3. **The ratchet.** The counts are compared against `baselines/` through
//!    the same [`arthron::gate::evaluate`] the `arthron gate` command uses, so
//!    a rate regression, or drift in either bucket that sits outside the rate,
//!    fails the build.
//!
//! Re-base deliberately, with the product's own command:
//!
//! ```text
//! arthron gate corpus/rust/ripgrep --language rust \
//!     --baseline baselines/rust-ripgrep.toml --rebase --commit e89fff8
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken resolver.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Lang, RefKind, node_id, reason_name};
use arthron::pipeline::source_files;
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_rust::extract::extract;
use arthron::track_rust::lang::RsLang;
use arthron::track_rust::resolve::scan_rust;

mod support;

const CORPUS: &str = "corpus/rust/ripgrep";
const BASELINE: &str = "baselines/rust-ripgrep.toml";

/// Every unresolved reason ripgrep produces, exactly.
///
/// Thirteen references, which is what makes pinning them cheap and floors
/// useless: `unresolved > 0` holds while twelve of the thirteen are
/// relabelled. `AliasCycle` is a `use` chain that re-enters itself and
/// `NoMatchingDefinition` a path into a module this walk did not index —
/// different facts, and the baseline records neither.
const RIPGREP_REASONS: &[(&str, u64)] = &[("AliasCycle", 11), ("NoMatchingDefinition", 2)];

/// The files the scan owns. Exact, because everything below is a count over
/// this set and a census over a different file set is a different census.
const FILES: usize = 99;

/// Every definition the extractor emits over those 99 files, by kind.
///
/// Asserted **exactly**, and this is the assertion this file was missing.
/// Definitions are tier 2's other deliverable and the import rate cannot see
/// one of them: deleting the rule that emits `DefKind::Method` removes 2066
/// nodes — 54.6% of everything ripgrep declares — and moves no reference, no
/// bucket, no rate and no baseline. `defs > 0` and "at least as many modules
/// as files" both still held. A number that can only be wrong deliberately
/// is the only kind worth writing down.
///
/// `Module` is 137 rather than 99 because a file declares its own module and
/// the tree writes 38 inline `mod x { … }` blocks besides.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 742),
    (DefKind::Method, 2066),
    (DefKind::Type, 419),
    (DefKind::Const, 48),
    (DefKind::Var, 7),
    (DefKind::Constructor, 253),
    (DefKind::Module, 137),
    (DefKind::Alias, 109),
];

/// Definition nodes the store holds after merging, by kind.
///
/// Lower than [`DEFS`] wherever two files declare one path — `impl` blocks
/// for one type split across files, a `#[cfg]` pair. The two censuses are the
/// point: the extractor's says nothing was lost on the way in, the store's
/// says nothing was lost or over-merged on the way through, and a bug that
/// moved definitions between the two would have to move both.
///
/// `DefKind::Module` is absent because a module is filed as a *package* node
/// rather than a definition; those are [`PACKAGES`].
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Function, 732),
    (DefKind::Method, 2053),
    (DefKind::Type, 418),
    (DefKind::Const, 48),
    (DefKind::Var, 7),
    (DefKind::Constructor, 253),
    (DefKind::Alias, 109),
];

/// Package nodes: one per module the tree declares, the 99 file-level ones
/// and the inline blocks together.
const PACKAGES: u64 = 137;

/// External nodes: the crates.io dependencies ripgrep's members name and this
/// scan does not index.
const EXTERNALS: u64 = 21;

/// Named definitions, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the shape. `DecompressionMatcher.new`
/// cannot be right unless an inherent `impl` block was attributed to the type
/// it implements, `ParseSizeErrorKind.InvalidInt` unless an enum variant is a
/// constructor rather than a field, and `flags#Flag` unless the crate root a
/// module hangs off is `crates/core/main.rs` — a binary target, not the
/// workspace root, which is the fact ripgrep's ten members exist here to
/// test.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    (
        "crates/cli/src/lib.rs::decompress#DecompressionMatcher",
        NodeKind::Definition(DefKind::Type),
        "crates/cli/src/decompress.rs",
        147,
    ),
    (
        "crates/cli/src/lib.rs::decompress#DecompressionMatcher.new",
        NodeKind::Definition(DefKind::Method),
        "crates/cli/src/decompress.rs",
        167,
    ),
    (
        "crates/cli/src/lib.rs::human#parse_human_readable_size",
        NodeKind::Definition(DefKind::Function),
        "crates/cli/src/human.rs",
        79,
    ),
    (
        "crates/cli/src/lib.rs::human#ParseSizeErrorKind.InvalidInt",
        NodeKind::Definition(DefKind::Constructor),
        "crates/cli/src/human.rs",
        14,
    ),
    (
        "crates/core/main.rs::flags#Flag",
        NodeKind::Definition(DefKind::Type),
        "crates/core/flags/mod.rs",
        71,
    ),
    // A `pub use` re-export: an alias key, which is why its site carries no
    // line of its own.
    (
        "crates/core/main.rs::flags#GenerateMode",
        NodeKind::Definition(DefKind::Alias),
        "crates/core/flags/mod.rs",
        0,
    ),
    // The module a file declares, filed as a package and not a definition.
    (
        "crates/regex/src/lib.rs::literal",
        NodeKind::Package,
        "crates/regex/src/literal.rs",
        1,
    ),
];

#[test]
fn the_extractor_reads_the_rust_corpus_without_losing_its_invariants() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        support::missing(corpus);
        return;
    }
    let files = source_files::<RsLang>(corpus).expect("walking the corpus");
    assert!(!files.is_empty(), "the corpus has no .rs files");

    let mut files_read = 0u64;
    let mut defs = 0u64;
    let mut refs = 0u64;
    let mut by_def_kind: BTreeMap<u8, u64> = BTreeMap::new();
    let mut use_decls = 0u64;

    for path in &files {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(source) = std::fs::read_to_string(path) else {
            continue; // a file that is not UTF-8 is not Rust this build reads
        };
        let facts = extract(&rel, &source);
        files_read += 1;

        // Every file declares the module its definitions live in, whether or
        // not anything else parsed — and it is first, because that is what
        // the driver reads to source a file-scope import.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no module",
        );
        defs += facts.defs.len() as u64;
        refs += facts.refs.len() as u64;
        use_decls += facts.header.use_decls as u64;
        for def in &facts.defs {
            *by_def_kind.entry(def.kind.code()).or_default() += 1;
        }
        for r in &facts.refs {
            // The tier-2 contract, at the record level: import and module
            // references, and nothing else.
            assert_eq!(r.kind, RefKind::Import, "{rel}: {}", r.raw_target);
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
        }
        // One `use` declaration yields at least one leaf, so a declaration
        // with no reference would be a silently dropped import.
        assert!(
            facts.refs.len() as u64 >= facts.header.use_decls as u64,
            "{rel}: {} use declarations produced {} references",
            facts.header.use_decls,
            facts.refs.len(),
        );
    }

    println!("files {files_read}  defs {defs}  refs {refs}  use declarations {use_decls}");
    for (code, n) in &by_def_kind {
        println!(
            "  def kind {}: {n}",
            DefKind::from_code(*code).map_or("?", DefKind::name)
        );
    }

    // What stood here was `defs > 0 && refs > 0` and "at least as many
    // modules as files", and neither could fail: the loop above already
    // asserts that *every* file declares a module first, so 99 files force
    // both. An assertion implied by one already made is not a second check,
    // it is a second place to read a pass. The totals are pinned instead —
    // ripgrep is fixed test data, so each of these is a fact about this
    // extractor reading a fixed 99 files.
    assert_eq!(
        files_read, FILES as u64,
        "the walk found a different file set"
    );
    assert_eq!(defs, DEFS.iter().map(|(_, n)| n).sum::<u64>());
    assert_eq!(defs, 3781, "the definition total moved");
    assert_eq!(refs, 1073, "the reference total moved");
    assert_eq!(use_decls, 377, "the `use` declaration tally moved");
    // 137 and not 99: every file declares one module of its own, and the
    // corpus's 38 inline `mod x { … }` blocks declare the rest.
    assert_eq!(
        by_def_kind.get(&DefKind::Module.code()).copied(),
        Some(137),
        "the module census moved",
    );
}

#[test]
fn the_rust_definition_census_is_exact() {
    // The hole this closes, found by deleting rather than by reading: with
    // Rust's method extraction removed — 2066 of the 3763 definitions the
    // tree declares — `cargo test corpus_rust` still passed, every count in
    // it being either a reference count or a floor. Definitions are half of
    // what a tier-2 track delivers and the import rate is blind to all of
    // them, so they are asserted exactly here or nowhere.
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        support::missing(corpus);
        return;
    }
    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    scan_rust(corpus, &db).expect("the corpus scans");

    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    drop(store);
    assert_eq!(owned.len(), FILES, "the scan owned a different file set");

    // Re-extracted from disk with no resolver in sight, over exactly the
    // files the scan owned.
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        for def in &extract(rel, &source).defs {
            *kinds.entry(def.kind.code()).or_default() += 1;
        }
    }
    println!("extracted defs {kinds:?}");
    let want: BTreeMap<u8, u64> = DEFS.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        kinds, want,
        "the definition census moved; no rate, no bucket and no baseline can see it",
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
    println!("stored defs {stored:?} packages {packages} externals {externals}");
    let want: BTreeMap<u8, u64> = STORED.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "the stored definition census moved");
    assert_eq!(packages, PACKAGES, "the stored package census moved");
    assert_eq!(externals, EXTERNALS, "the stored external census moved");

    for (fqn, kind, file, line) in PINNED {
        let id = node_id(Lang::Rust.domain(), fqn);
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
fn the_rust_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        support::missing(corpus);
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_rust(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Rust.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "rust         resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    for (code, count) in &tally.unresolved {
        println!("             {} {count}", reason_name(*code));
    }
    support::assert_reasons(CORPUS, &tally.unresolved, RIPGREP_REASONS);

    // -- completeness -----------------------------------------------------

    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    drop(store);
    assert!(!owned.is_empty(), "the scan owned no file");

    let mut re_extracted = 0u64;
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        re_extracted += extract(rel, &source).refs.len() as u64;
    }

    let accounted =
        measured.resolved + measured.external + measured.local_binding + measured.unresolved;
    assert_eq!(
        accounted,
        re_extracted,
        "{} references were extracted from {} files but {accounted} were accounted for; \
         a resolver that drops a reference reports a better rate for less work",
        re_extracted,
        owned.len(),
    );

    // -- the tier ---------------------------------------------------------

    // `LocalBinding` is the reason a reference to a *local* carries, and tier
    // 2 emits no expression-level reference for a block to bind. Zero here is
    // the contract holding, not a bucket nobody filled — so it is asserted
    // rather than merely observed.
    assert_eq!(
        measured.local_binding, 0,
        "tier 2 has nothing a local can bind",
    );

    // The buckets that must not be the whole of it: a run where everything
    // landed in one accounts for every reference and still measures nothing.
    assert!(measured.resolved > 0, "nothing linked at all");
    assert!(measured.unresolved > 0, "no floor: every reason is empty");
    assert!(
        measured.external > 0,
        "nothing reached outside the repository"
    );
    // And the resolution has to be *cross-crate*, not a file resolving to
    // itself: ripgrep is a ten-directory workspace with 20 intra-workspace
    // `path = …` dependency edges, and a resolver that read none of them
    // would still pass every count above.
    assert!(
        measured.resolved > u64::from(owned.len() as u32),
        "fewer links than files: {} links over {} files",
        measured.resolved,
        owned.len(),
    );

    // -- the ratchet ------------------------------------------------------

    let text = std::fs::read_to_string(BASELINE).unwrap_or_else(|e| {
        panic!(
            "reading {BASELINE}: {e}; record it with \
             `arthron gate {CORPUS} --language rust --baseline {BASELINE} --rebase --commit <sha>`"
        )
    });
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Rust.name(),
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
