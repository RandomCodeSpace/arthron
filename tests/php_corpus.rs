//! Milestone acceptance for PHP: a real, honest **import**-resolution rate on
//! a real corpus, and a ratchet that holds it.
//!
//! The corpus is not vendored here — it lives in `RandomCodeSpace/arthron-corpus`
//! and is cloned into `./corpus` (gitignored). Skipping when it is absent is
//! correct: failing would make an unfetched corpus look like a broken engine.
//!
//! PHP is **tier 2**, so what this measures is not what a tier-1 corpus test
//! measures. The denominator is the `use` imports and nothing else; there are
//! no call sites in it, and there must not be — that is asserted here rather
//! than trusted, because a tier-2 track that started emitting calls would
//! grow a denominator no tier-2 resolver links.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{
    Counts, FORMAT, GateVerdict, evaluate, is_renderable, parse_baseline, render_baseline,
};
use arthron::model::{DefKind, Lang, RefKind, reason_name};
use arthron::pipeline::source_files;
use arthron::store::Store;
use arthron::track_php::extract::extract;
use arthron::track_php::lang::PhpLang;
use arthron::track_php::resolve::scan_php;

const CORPUS: &str = "corpus/php/guzzle";
const BASELINE: &str = "baselines/php-guzzle.toml";
/// The pinned corpus revision, for the baseline's provenance line.
const CORPUS_COMMIT: &str = "3aeea04";

/// Whether the corpus has been cloned in.
fn corpus_present(corpus: &Path) -> bool {
    if corpus.join("composer.json").is_file() {
        return true;
    }
    println!("SKIP: no corpus at {} — see README", corpus.display());
    false
}

/// Count the corpus's references by extracting it again, independently of the
/// pipeline.
///
/// Deliberately not "ask the pipeline how many it found": a reference lost
/// between the extractor and the store would vanish from both sides of the
/// comparison and the assertion would pass.
fn extracted_reference_count(corpus: &Path) -> u64 {
    let mut total = 0u64;
    for path in source_files::<PhpLang>(corpus).expect("walking the corpus") {
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
fn the_extractor_reads_the_php_corpus_without_losing_its_invariants() {
    let corpus = Path::new(CORPUS);
    if !corpus_present(corpus) {
        return;
    }
    let files = source_files::<PhpLang>(corpus).expect("walking the corpus");
    assert_eq!(files.len(), 131, "the pinned corpus is 131 `.php` files");

    let mut defs_by_kind: BTreeMap<&str, u64> = BTreeMap::new();
    let mut refs = 0u64;
    let mut namespaces = 0u64;
    for path in &files {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(path).expect("reading a corpus file");
        let facts = extract(&rel, &source);

        // Every file states the container its definitions live in, whether or
        // not it writes a `namespace` clause — a file that writes none lands
        // in the global namespace, which is a container with no name.
        assert!(
            !facts.header.namespaces.is_empty(),
            "{rel} declares no container",
        );
        assert_eq!(
            facts
                .defs
                .iter()
                .filter(|d| d.kind == DefKind::Module)
                .count(),
            facts.header.namespaces.len(),
            "{rel}: namespace clauses and module definitions disagree",
        );
        namespaces += facts.header.namespaces.len() as u64;

        for d in &facts.defs {
            *defs_by_kind.entry(d.kind.name()).or_default() += 1;
            if d.kind != DefKind::Module {
                assert!(!d.owner.is_empty(), "{rel}: {} states no namespace", d.name,);
            }
        }
        for r in &facts.refs {
            // The tier-2 contract, asserted on real code: one reference kind.
            assert_eq!(r.kind, RefKind::Import, "{rel}: {}", r.raw_target);
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
            assert!(r.argc.is_none(), "{rel}: {}", r.raw_target);
            let e = r
                .enclosing
                .as_ref()
                .expect("an import states its namespace");
            assert_eq!(e.kind, DefKind::Module);
        }
        refs += facts.refs.len() as u64;
    }

    println!(
        "files {}  namespaces {namespaces}  imports {refs}",
        files.len()
    );
    for (kind, n) in &defs_by_kind {
        println!("  def {kind:<12} {n}");
    }

    // One corpus file declares three namespaces with braced blocks and one
    // declares none, so the file-to-namespace mapping is not a function —
    // which is the whole reason a namespace is `owner[0]` and not a header
    // field.
    assert!(
        namespaces > files.len() as u64,
        "the braced-namespace file is not being read",
    );
    assert_eq!(refs, 795, "the corpus's file-scope `use` imports");

    // The exact definition tally, measured. A tier-2 track's structure half
    // is the half no rate defends, so it is asserted here or nowhere.
    assert_eq!(
        defs_by_kind.into_iter().collect::<Vec<_>>(),
        [
            // Class constants and enum cases; the corpus declares no
            // namespace-level `const`.
            ("const", 113),
            // Declared properties. The corpus is PHP 7.4-compatible and
            // promotes none, so every one of these is written out.
            ("field", 160),
            // Namespace-level functions, all of them `tests/bootstrap.php`'s
            // curl shims.
            ("function", 9),
            ("method", 2707),
            // Two more than there are files: one file declares three
            // namespaces with braced blocks.
            ("module", 133),
            // Classes, interfaces, traits and enums together.
            ("type", 159),
        ],
    );
}

#[test]
fn corpus_rate_is_nonzero_and_every_unresolved_has_a_reason() {
    let corpus = Path::new(CORPUS);
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.redb");
    let report = scan_php(corpus, &db).expect("scan");
    let php = &report.per_lang[&Lang::Php.code()];

    let unresolved = php.unresolved_total();
    let rate = arthron::resolution_rate(php.resolved, unresolved)
        .expect("the corpus has imports to measure");

    println!("php import rate {rate:.4}");
    println!("  resolved     {}", php.resolved);
    println!("  unresolved   {unresolved}");
    println!("  external     {}", php.external);
    println!("  localbinding {}", php.local_binding);
    for (code, count) in &php.unresolved {
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

    // The exact tally, measured. Not a floor: a change that moves any of
    // these four numbers is a change that has to say why.
    assert_eq!(php.resolved, 360);
    assert_eq!(php.external, 265);
    assert_eq!(
        php.local_binding, 0,
        "tier 2 has no expression-level reference to bind"
    );
    assert_eq!(unresolved, 170);
    assert_eq!(php.resolved + php.external + unresolved, 795);

    // One reason and one only, and it is the documented floor: a sibling
    // composer package under this repository's own vendor namespace root.
    // `NoMatchingDefinition` is the bucket reserved for meaning arthron's own
    // bug and `ProjectLayoutUnknown` for arthron's own blind spot; both being
    // absent is what makes the 170 a statement about composer rather than
    // about this extractor.
    assert_eq!(
        php.unresolved
            .iter()
            .map(|(code, n)| (reason_name(*code), *n))
            .collect::<Vec<_>>(),
        [("ModuleNotFound", 170)],
    );

    assert!(rate > 0.0, "nothing resolved");
    // Rates are per language and never aggregated: Go must not appear in a
    // report produced by the PHP track's own scan of a PHP-only tree.
    assert!(!report.per_lang.contains_key(&Lang::Go.code()));
}

/// Measure the corpus once against a cold store.
///
/// The ratchet and the recorder share it so that the file one writes is the
/// number the other compares.
fn measure(corpus: &Path) -> Counts {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let report = scan_php(corpus, &dir.path().join("graph.redb")).expect("scan");
    let php = &report.per_lang[&Lang::Php.code()];
    Counts {
        resolved: php.resolved,
        external: php.external,
        local_binding: php.local_binding,
        unresolved: php.unresolved_total(),
    }
}

#[test]
fn the_ratchet_holds() {
    let root = Path::new(CORPUS);
    if !corpus_present(root) {
        return;
    }
    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Php.name(),
        "{BASELINE} measures another language; rates are per language and never aggregated",
    );
    assert_eq!(
        baseline.corpus, CORPUS,
        "{BASELINE} was recorded from another corpus"
    );

    let measured = measure(root);
    println!(
        "{CORPUS}: resolved {} external {} local-binding {} unresolved {}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    match evaluate(&baseline, &measured) {
        GateVerdict::Pass { .. } => {}
        other => panic!("{BASELINE}: {other:?}\nmeasured {measured:?}"),
    }
}

/// The baseline is written by `arthron gate --rebase` and by nothing else:
///
/// ```text
/// arthron gate corpus/php/guzzle --language php \
///     --baseline baselines/php-guzzle.toml --rebase --commit 3aeea04
/// ```
#[test]
fn the_php_baseline_names_the_corpus_it_measures() {
    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(baseline.corpus, CORPUS);
    assert_eq!(baseline.commit, CORPUS_COMMIT);
    assert_eq!(baseline.language, Lang::Php.name());
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
