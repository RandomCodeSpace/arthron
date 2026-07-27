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
use arthron::model::{DefKind, Lang, RefKind, reason_name};
use arthron::pipeline::source_files;
use arthron::store::Store;
use arthron::track_rust::extract::extract;
use arthron::track_rust::lang::RsLang;
use arthron::track_rust::resolve::scan_rust;

const CORPUS: &str = "corpus/rust/ripgrep";
const BASELINE: &str = "baselines/rust-ripgrep.toml";

#[test]
fn the_extractor_reads_the_rust_corpus_without_losing_its_invariants() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
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
    assert!(defs > 0 && refs > 0);
    // Every file declares one module of its own, and the corpus's 38 inline
    // `mod x { … }` blocks declare more — so the module count is the file
    // count plus whatever is written, never fewer.
    assert!(
        by_def_kind
            .get(&DefKind::Module.code())
            .copied()
            .unwrap_or(0)
            >= files_read,
        "fewer modules than files",
    );
}

#[test]
fn the_rust_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
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
