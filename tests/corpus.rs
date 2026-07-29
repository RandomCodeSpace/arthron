//! Milestone acceptance: a non-zero, honest resolution rate on the corpus.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use arthron::extract_go::extract;
use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Lang, RefKind, node_id, reason_name};
use arthron::pipeline::{scan_go, source_files};
use arthron::query::{NodeKind, definition};
use arthron::resolve_go::GoLang;
use arthron::store::{NodeRecord, ReadStore, Store, StoredOutcome};

mod support;

/// Whether the corpus has been cloned in.
///
/// It lives in RandomCodeSpace/arthron-corpus, cloned into ./corpus
/// (gitignored). Skipping is correct when it is absent — failing would make
/// an unfetched corpus look like a broken engine.
fn corpus_present(corpus: &Path) -> bool {
    if corpus.join("go.mod").is_file() {
        return true;
    }
    support::missing(corpus);
    false
}

/// Count the references in the corpus by extracting it again, independently
/// of the pipeline.
///
/// This deliberately does not ask the pipeline how many references it found:
/// a bug that loses one between the extractor and the store would lose it
/// from both sides of the comparison and the assertion would pass. It shares
/// only the two things it must in order to be comparing the same corpus at
/// all — [`extract`], and [`source_files`] for the file set.
fn extracted_reference_count(corpus: &Path) -> u64 {
    let mut total = 0u64;
    for path in source_files::<GoLang>(corpus).expect("walking the corpus") {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let facts = extract(&rel, &source);
        total += facts.refs.len() as u64;
    }
    total
}

#[test]
fn corpus_rate_is_nonzero_and_every_unresolved_has_a_reason() {
    let corpus = Path::new("corpus/go/codeiq");
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let report = scan_go(corpus, &dir.path().join("graph.redb")).expect("scan");
    let go = &report.per_lang[&Lang::Go.code()];

    let unresolved = go.unresolved_total();
    let rate = arthron::resolution_rate(go.resolved, unresolved)
        .expect("the corpus has references to measure");

    println!(
        "resolved {} external {} local-binding {} unresolved {}",
        go.resolved, go.external, go.local_binding, unresolved
    );
    for (code, count) in &go.unresolved {
        println!("  {}: {count}", reason_name(*code));
    }
    println!("rate {:.1}%", rate * 100.0);

    // The definition of done: non-zero and honest. The predecessor's
    // baseline on this exact code was 0.0%.
    assert!(rate > 0.0, "resolution rate must beat the 0% baseline");
    assert!(
        unresolved > 0,
        "a skeleton claiming 100% is lying somewhere"
    );

    // `assert!(go.resolved > 0)` stood here and could not fail: the rate is
    // `resolved / (resolved + unresolved)` and the line above already
    // requires it to be positive, so the two assertions are one. The half of
    // this test's name that nothing checked is the *second* half — that
    // every unresolved reference carries a reason — and that is what
    // replaces it.
    assert!(
        !go.unresolved.is_empty(),
        "unresolved with no reason at all"
    );
    for (code, count) in &go.unresolved {
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
    // The floor, named. Go's resolver runs no type checker, so a receiver
    // whose type is not stated in the file is honestly unresolved — and a
    // scan reporting none of those would have moved them into `external` or
    // `local_binding`, which sit outside both terms of the rate.
    assert!(
        go.unresolved
            .iter()
            .any(|(code, n)| reason_name(*code) == "NeedsTypeInference" && *n > 0),
        "no inference floor: it was reclassified, not resolved",
    );
    // And the whole tally, exactly. The floor above is the weaker half of
    // this: it survives any relabelling that leaves one reference behind, and
    // relabelling costs nothing — none of the four gated integers moves when
    // an unresolved reference changes reason.
    support::assert_reasons("corpus/go/codeiq", &go.unresolved, CODEIQ_REASONS);
}

#[test]
fn every_corpus_reference_has_exactly_one_stored_outcome() {
    // "The resolver never drops" is the project's central claim, and a rate
    // is no evidence for it: silently discarding the references it cannot
    // link would *raise* the rate. The reported columns partition the
    // extracted references, so their sum is the reference count — exactly.
    // Under-counting is a dropped reference; over-counting is one reference
    // reported as two outcomes. Both break the contract.
    //
    // `local_binding` is one of the columns even though it is outside both
    // terms of the rate: it is excluded from the *measurement*, never from
    // the *accounting*. Leaving it out here is precisely how moving
    // references into it could look like an improvement.
    let corpus = Path::new("corpus/go/codeiq");
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let report = scan_go(corpus, &dir.path().join("graph.redb")).expect("scan");
    let go = &report.per_lang[&Lang::Go.code()];

    let stored = go.resolved + go.external + go.local_binding + go.unresolved_total();
    let extracted = extracted_reference_count(corpus);
    println!("stored outcomes {stored}, extracted references {extracted}");
    assert_eq!(
        stored,
        extracted,
        "resolved {} + external {} + local-binding {} + unresolved {} must \
         equal the {extracted} references the extractor found — every \
         reference gets exactly one stored outcome",
        go.resolved,
        go.external,
        go.local_binding,
        go.unresolved_total(),
    );
}

// -- the definition census -------------------------------------------------
//
// Everything above this line is a statement about *references*: a rate, a
// partition, and a warm store that agrees with a cold one. None of them can
// see a definition go missing. Deleting the rule that emits Go's methods
// removes 614 nodes from codeiq and 1425 from caddy, and moves no rate, no
// bucket, no baseline and no incremental oracle — every one of those compares
// two things that would lose the definition together. So the definitions are
// counted here, exactly, on both sides of the store, with named nodes beside
// the totals because a census pins the scale and only a name pins the shape.

/// The measurement one Go corpus's census is: `(files, extracted, stored,
/// packages, externals, pinned)`.
struct Census {
    files: usize,
    defs: &'static [(DefKind, u64)],
    stored: &'static [(DefKind, u64)],
    packages: u64,
    externals: u64,
    pinned: &'static [(&'static str, NodeKind, &'static str, u32)],
}

/// codeiq: 397 files, and the tree the resolution rate is quoted from.
const CODEIQ: Census = Census {
    files: 397,
    // `Module` is one per file: every file declares the package its
    // definitions live in, whether or not anything else parsed.
    //
    // `Type` counts 232 and not 229 because `def-type` now reads `type_alias`
    // as well as `type_spec`: codeiq writes exactly three package-level
    // `type X = Y` declarations, and until Go emitted type uses nothing in
    // the tree could tell that they declared no node.
    defs: &[
        (DefKind::Function, 1545),
        (DefKind::Method, 614),
        (DefKind::Type, 232),
        (DefKind::Const, 276),
        (DefKind::Var, 619),
        (DefKind::Module, 397),
    ],
    // Lower than the extractor's on `Function` alone: a package's `init` is
    // written in several files and is one identity. `Module` is absent
    // because a package is filed as a package node, counted below.
    stored: &[
        (DefKind::Function, 1429),
        (DefKind::Method, 614),
        (DefKind::Type, 232),
        (DefKind::Const, 276),
        (DefKind::Var, 619),
    ],
    packages: 50,
    // 84 and not 81: `io/fs`, `sync` and `testing` are imported here and
    // *used* only in type position — `fs.FS`, `sync.Mutex`, `testing.T` —
    // so the store held `std:io/fs` for the import and nothing for the use
    // until type uses were emitted. Every other new Go reference in this
    // corpus reached a package it already had a node for.
    //
    // 85 and not 84 since field reads: `syscall` is imported here and *named*
    // only as a value (`syscall.SIGTERM`), so the store held `std:syscall` for
    // the import and nothing for the use. An external package reached by a
    // qualified reference is filed under its bare path and one reached by an
    // import under `std:`, which is why the two are different nodes.
    externals: 85,
    pinned: &[
        (
            "github.com/randomcodespace/codeiq/internal/analyzer#Analyzer",
            NodeKind::Definition(DefKind::Type),
            "internal/analyzer/analyzer.go",
            27,
        ),
        // A method, which is the kind a receiver has to be read to file
        // correctly: `Analyzer.Run` under the type and not beside it.
        (
            "github.com/randomcodespace/codeiq/internal/analyzer#Analyzer.Run",
            NodeKind::Definition(DefKind::Method),
            "internal/analyzer/analyzer.go",
            63,
        ),
        (
            "github.com/randomcodespace/codeiq/internal/analyzer#NewAnalyzer",
            NodeKind::Definition(DefKind::Function),
            "internal/analyzer/analyzer.go",
            32,
        ),
        (
            "github.com/randomcodespace/codeiq/internal/analyzer#DefaultBatchSize",
            NodeKind::Definition(DefKind::Const),
            "internal/analyzer/analyzer.go",
            16,
        ),
        // A package-level `var`, declared in a different file of the same
        // package: the container is the directory, not the file.
        (
            "github.com/randomcodespace/codeiq/internal/analyzer#DefaultExcludeDirs",
            NodeKind::Definition(DefKind::Var),
            "internal/analyzer/file_discovery.go",
            15,
        ),
        (
            "github.com/randomcodespace/codeiq/cmd/extcheck",
            NodeKind::Package,
            "cmd/extcheck/main.go",
            1,
        ),
        // An alias, and the one shape the widened `def-type` rule added: it
        // is a node of kind `Type`, so a use of `Node` reaches it rather than
        // landing on `NoMatchingDefinition`. Named, because the census only
        // says three appeared.
        (
            "github.com/randomcodespace/codeiq/internal/parser#Node",
            NodeKind::Definition(DefKind::Type),
            "internal/parser/walk.go",
            13,
        ),
    ],
};

/// caddy: 314 files, and the tree whose definitions collide.
const CADDY: Census = Census {
    files: 314,
    // `Type` counts 511 and not 507 for the same reason codeiq's counts 232:
    // caddy writes exactly four package-level `type X = Y` declarations, and
    // `def-type` reads `type_alias` now.
    defs: &[
        (DefKind::Function, 1139),
        (DefKind::Method, 1425),
        (DefKind::Type, 511),
        (DefKind::Const, 170),
        (DefKind::Var, 546),
        (DefKind::Module, 314),
    ],
    // The merge is loud here and that is the point of the second corpus:
    // 546 `var` declarations become 209 identities, because caddy writes
    // `var _ Module = (*T)(nil)` interface guards in most of its files and
    // every one of them declares the blank identifier in its own package.
    // A count that only ever went up would not notice them collapsing.
    stored: &[
        (DefKind::Function, 1009),
        (DefKind::Method, 1425),
        (DefKind::Type, 511),
        (DefKind::Const, 169),
        (DefKind::Var, 209),
    ],
    packages: 47,
    // 249 and not 241, and the same shape as codeiq's 84: `crypto`,
    // `crypto/ed25519`, `crypto/rsa`, `crypto/x509/pkix`, `flag`, `hash`,
    // `math/big` and `sync/atomic` are each imported and used only as a type.
    //
    // 251 and not 249 since field reads, and the same shape as codeiq's 85:
    // `net/http/pprof` and `unicode/utf8` are each imported and named only as
    // a value — `pprof.Index`, `utf8.RuneSelf`.
    externals: 251,
    pinned: &[
        (
            "github.com/caddyserver/caddy/v2/modules/caddyhttp#Server",
            NodeKind::Definition(DefKind::Type),
            "modules/caddyhttp/server.go",
            47,
        ),
        (
            "github.com/caddyserver/caddy/v2#Module",
            NodeKind::Definition(DefKind::Type),
            "modules.go",
            54,
        ),
        (
            "github.com/caddyserver/caddy/v2#APIError.Error",
            NodeKind::Definition(DefKind::Method),
            "admin.go",
            1382,
        ),
        (
            "github.com/caddyserver/caddy/v2#AppConfigDir",
            NodeKind::Definition(DefKind::Function),
            "storage.go",
            86,
        ),
        (
            "github.com/caddyserver/caddy/v2#DefaultLoggerName",
            NodeKind::Definition(DefKind::Const),
            "logging.go",
            817,
        ),
        (
            "github.com/caddyserver/caddy/v2#ConfigAutosavePath",
            NodeKind::Definition(DefKind::Var),
            "storage.go",
            157,
        ),
        (
            "github.com/caddyserver/caddy/v2/modules/caddyhttp/push",
            NodeKind::Package,
            "modules/caddyhttp/push/caddyfile.go",
            15,
        ),
        // caddy's half of the alias pin — see codeiq's `#Node`.
        (
            "github.com/caddyserver/caddy/v2/modules/caddyhttp#LoggableHTTPHeader",
            NodeKind::Definition(DefKind::Type),
            "modules/caddyhttp/marshalers.go",
            65,
        ),
    ],
};

/// Count the definitions on both sides of the store and compare them, node
/// for node, with what this corpus's [`Census`] records.
fn assert_census(corpus: &str, census: &Census) {
    let root = Path::new(corpus);
    if !corpus_present(root) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.redb");
    scan_go(root, &db).expect("scan");

    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    drop(store);
    assert_eq!(
        owned.len(),
        census.files,
        "{corpus}: the scan owned a different file set",
    );

    // Re-extracted from disk over exactly the files the scan owned, with no
    // resolver in sight: the extractor's own answer, not the store's.
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
        let id = node_id(Lang::Go.domain(), fqn);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("{fqn} is not in the store"));
        assert_eq!(def.node.name, *fqn);
        assert_eq!(def.node.kind, *kind, "{fqn}");
        // A package is declared by every file in its directory, so only the
        // sites in the file this pin names are worth printing when it misses.
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

// -- the reference census ---------------------------------------------------
//
// The definition census above cannot see a *reference* rule go missing, and
// neither can anything else here: the ratchet compares four occurrence totals,
// so deleting one rule and gaining rows from another passes it, and the reason
// tally is blind to a kind that stops being emitted at all. `ref-select` and
// `ref-litkey` are 11,334 occurrences on codeiq and 13,723 on caddy, and every
// one of them would vanish under a green suite without this. So each kind is
// counted, exactly, on both axes — rows and occurrences, because a rule that
// stops deduplicating moves one and not the other — with named rows beside the
// totals, since a census pins the scale and only a name pins the shape.

/// One corpus's reference surface: `(kind name, rows, occurrences)` for every
/// kind it emits, and the rows worth naming.
struct RefCensus {
    /// Every [`RefKind`] the corpus produces, with its exact row and
    /// occurrence counts. A kind with no rows is absent, and a kind that
    /// appears here with zero would be a contradiction.
    kinds: &'static [(&'static str, u64, u64)],
    /// `(file, site text, outcome)`, where the outcome is rendered as the
    /// *name* it reaches — so a reference linked to the wrong node fails here
    /// rather than counting as one more `resolved`.
    pinned: &'static [(&'static str, &'static str, &'static str)],
}

/// codeiq's, exactly.
const CODEIQ_REFS: RefCensus = RefCensus {
    kinds: &[
        ("call", 9271, 13933),
        ("import", 1694, 1694),
        ("type-use", 6401, 9596),
        ("field-access", 7436, 11334),
    ],
    pinned: &[
        // A method value reached through the method's own receiver: `this.m`
        // with no call, resolved from the type the signature states.
        (
            "internal/graph/indexes.go",
            "s.searchByLabelFallback",
            "resolved github.com/randomcodespace/codeiq/internal/graph#Store.searchByLabelFallback",
        ),
        // A package-qualified value read — the shape that was resolvable all
        // along and simply not emitted.
        (
            "internal/analyzer/file_discovery.go",
            "parser.LanguageUnknown",
            "resolved github.com/randomcodespace/codeiq/internal/parser#LanguageUnknown",
        ),
        // A struct literal's key, carried as the target it names rather than
        // the bare `Files` the site writes: two literals of two types in one
        // function would otherwise share a row and an outcome.
        (
            "internal/analyzer/analyzer.go",
            "Stats.Files",
            "NeedsReceiverType",
        ),
        ("internal/analyzer/analyzer.go", "os.Stderr", "external os"),
    ],
};

/// caddy's, exactly.
const CADDY_REFS: RefCensus = RefCensus {
    kinds: &[
        ("call", 13979, 21388),
        ("import", 2429, 2429),
        ("type-use", 11015, 16544),
        ("field-access", 8332, 13723),
    ],
    pinned: &[
        (
            "modules/caddyhttp/celmatcher.go",
            "m.caddyPlaceholderFunc",
            "resolved github.com/caddyserver/caddy/v2/modules/caddyhttp#MatchExpression.caddyPlaceholderFunc",
        ),
        (
            "caddyconfig/httpcaddyfile/builtins.go",
            "caddy.DefaultLoggerName",
            "resolved github.com/caddyserver/caddy/v2#DefaultLoggerName",
        ),
        // A struct literal's key on a *dependency's* type: the type is stated
        // and it is outside this repository, so the key is a link out of it
        // and sits outside both terms of the rate.
        (
            "admin.go",
            "certmagic.Config.Logger",
            "external github.com/caddyserver/certmagic",
        ),
        ("admin.go", "adminHandler.mux", "NeedsReceiverType"),
    ],
};

/// Count every stored reference row by kind, and read the named ones back as
/// the node they reach.
fn assert_ref_census(corpus: &str, census: &RefCensus) {
    let root = Path::new(corpus);
    if !corpus_present(root) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.redb");
    scan_go(root, &db).expect("scan");

    let store = Store::open(&db).expect("store opens");
    let snapshot = store.snapshot().expect("snapshot");
    let names: BTreeMap<_, _> = snapshot
        .nodes
        .iter()
        .filter_map(|(id, record)| match record {
            NodeRecord::Definition { fqn, .. } => Some((*id, fqn.clone())),
            NodeRecord::Package { import_path, .. } => Some((*id, import_path.clone())),
            NodeRecord::External { .. } => None,
        })
        .collect();
    let render = |record: &arthron::store::RefRecord| match &record.outcome {
        StoredOutcome::Resolved(id) => format!(
            "resolved {}",
            names.get(id).map_or("<unnamed node>", String::as_str)
        ),
        StoredOutcome::External(package) => format!("external {package}"),
        StoredOutcome::Unresolved(code) => reason_name(*code).to_string(),
    };

    let mut counted: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for (key, record) in &snapshot.rows {
        let kind = RefKind::from_code(key.kind)
            .unwrap_or_else(|| panic!("{corpus}: row kind {} names no variant", key.kind));
        let entry = counted.entry(kind.name()).or_default();
        entry.0 += 1;
        entry.1 += u64::from(record.count);
    }
    println!("{corpus}: reference rows by kind {counted:?}");
    let want: BTreeMap<&str, (u64, u64)> = census
        .kinds
        .iter()
        .map(|(name, rows, occurrences)| (*name, (*rows, *occurrences)))
        .collect();
    assert_eq!(
        counted, want,
        "{corpus}: the reference census moved — a kind that stops being \
         emitted moves no baseline and no reason bucket",
    );

    for (file, raw_target, outcome) in census.pinned {
        let hits: Vec<String> = snapshot
            .rows
            .iter()
            .filter(|(key, _)| key.file == *file && key.raw_target == *raw_target)
            .map(|(_, record)| render(record))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "{corpus}: expected one `{raw_target}` row in {file}, found {hits:?}",
        );
        assert_eq!(&hits[0], outcome, "{corpus}: {file} `{raw_target}`");
    }
}

#[test]
fn the_codeiq_reference_census_is_exact() {
    assert_ref_census("corpus/go/codeiq", &CODEIQ_REFS);
}

#[test]
fn the_caddy_reference_census_is_exact() {
    assert_ref_census("corpus/go/caddy", &CADDY_REFS);
}

#[test]
fn the_codeiq_definition_census_is_exact() {
    assert_census("corpus/go/codeiq", &CODEIQ);
}

#[test]
fn the_caddy_definition_census_is_exact() {
    assert_census("corpus/go/caddy", &CADDY);
}

/// Copy a tree so an event has something to edit.
///
/// `corpus/` is pinned test data and is never written to; every file event
/// below happens to a copy.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap_or_else(|e| panic!("creating {}: {e}", to.display()));
    for entry in fs::read_dir(from).unwrap_or_else(|e| panic!("reading {}: {e}", from.display())) {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target)
                .unwrap_or_else(|e| panic!("copying {}: {e}", entry.path().display()));
        }
    }
}

/// The largest Go file in the tree: a deterministic pick, and the one whose
/// definitions the rest of the corpus is likeliest to reference.
fn largest_file(root: &Path) -> PathBuf {
    source_files::<GoLang>(root)
        .expect("walking the tree")
        .into_iter()
        .max_by_key(|path| {
            let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            // Path breaks the tie, so the pick does not depend on walk order.
            (size, path.clone())
        })
        .expect("the corpus has Go files")
}

/// Scan `tree` cold into a throwaway store and compare it, whole, with what
/// the incremental scans left in `warm_db`.
///
/// Compared **after every event**, not once at the end: a delete followed by
/// a restore puts every identity back, so a store that went stale in between
/// looks correct again by the time the sequence finishes. The intermediate
/// state is the one that has anything to say.
fn assert_matches_cold(tree: &Path, warm_db: &Path, event: &str) {
    let cold_dir = tempfile::tempdir().unwrap();
    let cold_db = cold_dir.path().join("cold.redb");
    let cold_report = scan_go(tree, &cold_db).expect("cold scan");
    let cold = Store::open(&cold_db)
        .expect("open cold")
        .snapshot()
        .unwrap();

    let warm_store = Store::open(warm_db).expect("open warm");
    let warm = warm_store.snapshot().unwrap();
    let warm_report = warm_store.report().unwrap();

    println!(
        "after {event}: {} files, {} nodes, {} rows, {} edges",
        warm.files.len(),
        warm.nodes.len(),
        warm.rows.len(),
        warm.edges.len(),
    );
    assert_eq!(
        cold.files.len(),
        warm.files.len(),
        "after {event}, known files: cold {} warm {}",
        cold.files.len(),
        warm.files.len(),
    );
    for (key, value) in &cold.rows {
        match warm.rows.get(key) {
            None => panic!("after {event}, a cold scan holds row {key:?} => {value:?}"),
            Some(w) => assert!(
                w == value,
                "after {event}, row {key:?}\n  cold {value:?}\n  warm {w:?}"
            ),
        }
    }
    let extra: Vec<_> = warm
        .rows
        .keys()
        .filter(|k| !cold.rows.contains_key(k))
        .collect();
    assert!(
        extra.is_empty(),
        "after {event}, rows a cold scan does not hold: {extra:?}",
    );
    for (id, record) in &cold.nodes {
        assert_eq!(
            warm.nodes.get(id),
            Some(record),
            "after {event}, node {id:?}"
        );
    }
    assert_eq!(cold.nodes.len(), warm.nodes.len(), "after {event}");
    assert_eq!(cold.edges, warm.edges, "after {event}");
    assert_eq!(cold.candidates, warm.candidates, "after {event}");
    assert_eq!(cold, warm, "after {event}, the snapshots differ");
    assert_eq!(cold_report, warm_report, "after {event}");
}

#[test]
fn an_incremental_event_on_the_corpus_lands_a_cold_scans_store() {
    // The oracle at real scale. `tests/incremental.rs` proves the same
    // property on four hand-written files, where every candidate is visible
    // by eye; this proves it where a stale row, a dangling candidate entry
    // or a node one file too many declares would hide.
    let corpus = Path::new("corpus/go/codeiq");
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    copy_tree(corpus, &tree);

    let warm_db = dir.path().join("warm.redb");
    scan_go(&tree, &warm_db).expect("first scan");

    let victim = largest_file(&tree);
    let rel = victim.strip_prefix(&tree).unwrap().display().to_string();
    let original = fs::read_to_string(&victim).expect("reading the file to edit");

    // A hash change that changes no fact: nothing may be woken, and nothing
    // may be lost either.
    fs::write(
        &victim,
        format!("{original}\n// arthron incremental oracle\n"),
    )
    .expect("touching the file");
    scan_go(&tree, &warm_db).expect("touch");
    assert_matches_cold(&tree, &warm_db, &format!("a comment appended to {rel}"));

    // Every definition in the busiest file of the corpus disappears at once.
    // Whatever referenced them sits in files this event never reads as
    // changed — the candidate index is the only thing that can name them.
    fs::remove_file(&victim).expect("deleting the file");
    scan_go(&tree, &warm_db).expect("delete");
    assert_matches_cold(&tree, &warm_db, &format!("{rel} deleted"));

    // And comes back, byte for byte: every identity the delete destroyed is
    // created again, and the references that went unresolved have to find it.
    fs::write(&victim, &original).expect("restoring the file");
    scan_go(&tree, &warm_db).expect("restore");
    assert_matches_cold(&tree, &warm_db, &format!("{rel} restored"));
}

#[test]
fn deleting_a_file_from_the_collision_corpus_lands_a_cold_scans_store() {
    // The same oracle against the corpus whose definitions collide: caddy
    // holds 28 FQNs that two files each declare. A node one file too many
    // declares, or one too few, is invisible in every tally — they are
    // summed from per-file rows — and the per-file replace path is exactly
    // where such a node is either kept or lost.
    let corpus = Path::new("corpus/go/caddy");
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    copy_tree(corpus, &tree);

    let warm_db = dir.path().join("warm.redb");
    scan_go(&tree, &warm_db).expect("first scan");

    let victim = largest_file(&tree);
    let rel = victim.strip_prefix(&tree).unwrap().display().to_string();
    fs::remove_file(&victim).expect("deleting the file");
    scan_go(&tree, &warm_db).expect("delete");
    assert_matches_cold(&tree, &warm_db, &format!("{rel} deleted"));
}

/// Compare one Go corpus against its committed baseline.
///
/// The ratchet is the project's own, reused rather than reimplemented:
/// [`parse_baseline`] reads the file `arthron gate --rebase` wrote and
/// [`evaluate`] performs the same exact integer comparison the command does.
/// Running it here as well as in CI is what makes a rate regression — or
/// drift in `external` or `local_binding`, the two columns that sit outside
/// *both* terms of the rate and are therefore the one way the gate could be
/// raised without anything being linked — fail `cargo test` wherever the
/// corpus is present.
///
/// Every unresolved reason codeiq produces, exactly.
///
/// `NeedsReceiverType` is now the largest, and that is the honest shape of
/// this build: a member named on a type that *is* stated — through a method's
/// receiver, through a struct literal's own type, or through a package's type
/// — which this track cannot find because it indexes neither Go embedding nor
/// struct fields. Go struct fields are not nodes here, so every field read and
/// every struct-literal key lands in it.
///
/// `NeedsTypeInference` is the name whose type nobody wrote down, and
/// `NeedsExpressionType` the operand that is not a name at all (`f().x`,
/// `m[k].x`) — a distinction the taxonomy already carried and that nothing in
/// Go had produced until reads were emitted.
///
/// `NoMatchingDefinition` is **absent, and that is the assertion**. Its
/// contract is that the lookup table was complete and the name absent, which
/// in a corpus that compiles means arthron's own bug — and every one of the
/// 123 rows it held here was a predeclared type name at a conversion
/// (`string(b)`, `int64(n)`), a name that is not absent at all. An empty
/// bucket is what that contract looks like when it is kept.
const CODEIQ_REASONS: &[(&str, u64)] = &[
    ("NeedsReceiverType", 3116),
    ("NeedsTypeInference", 770),
    ("NeedsExpressionType", 409),
];

/// caddy's, exactly. Twice the tree and the same three reasons in the same
/// proportion, which is the point of measuring two corpora — and the same
/// empty `NoMatchingDefinition`, from 269 rows of the same one cause.
const CADDY_REASONS: &[(&str, u64)] = &[
    ("NeedsReceiverType", 5852),
    ("NeedsTypeInference", 2855),
    ("NeedsExpressionType", 307),
];

/// One baseline per corpus, never one aggregated number. They are written by
/// the command and by nothing else:
///
/// ```text
/// arthron gate corpus/go/codeiq --language go \
///     --baseline baselines/go-codeiq.toml --rebase --commit 853efde
/// arthron gate corpus/go/caddy  --language go \
///     --baseline baselines/go-caddy.toml  --rebase --commit 853efde
/// ```
fn assert_ratchet(corpus: &str, baseline_path: &str, reasons: &[(&str, u64)]) {
    let root = Path::new(corpus);
    if !corpus_present(root) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let report = scan_go(root, &dir.path().join("graph.redb")).expect("scan");
    let go = &report.per_lang[&Lang::Go.code()];
    let measured = Counts {
        resolved: go.resolved,
        external: go.external,
        local_binding: go.local_binding,
        unresolved: go.unresolved_total(),
    };
    println!(
        "{corpus}: resolved {} external {} local-binding {} unresolved {}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    for (code, count) in &go.unresolved {
        println!("  {}: {count}", reason_name(*code));
    }
    // Exactly, not as a floor: the four numbers `evaluate` compares below are
    // the same whichever reason each unresolved reference carries, so a
    // relabelled bucket passes this ratchet untouched.
    support::assert_reasons(corpus, &go.unresolved, reasons);

    let text = std::fs::read_to_string(baseline_path)
        .unwrap_or_else(|e| panic!("reading {baseline_path}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{baseline_path}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Go.name(),
        "{baseline_path} measures another language; rates are per language and never aggregated",
    );
    assert_eq!(
        baseline.corpus, corpus,
        "{baseline_path} was recorded from another corpus",
    );
    match evaluate(&baseline, &measured) {
        GateVerdict::Pass { improved } => {
            if improved {
                println!("gate: pass — improved on {baseline_path}; re-base to move the ratchet");
            }
        }
        GateVerdict::Fail(failures) => {
            panic!("{baseline_path}: {failures:?}\nmeasured {measured:?}")
        }
        GateVerdict::Error(e) => panic!("{baseline_path}: {e}"),
    }
}

#[test]
fn go_holds_its_baseline_on_codeiq() {
    assert_ratchet(
        "corpus/go/codeiq",
        "baselines/go-codeiq.toml",
        CODEIQ_REASONS,
    );
}

#[test]
fn go_holds_its_baseline_on_caddy() {
    // The second Go corpus, and not a formality: caddy holds 28 FQNs that two
    // files each declare, and its local-binding column is three times
    // codeiq's. A single corpus locks in a number rather than a capability.
    assert_ratchet("corpus/go/caddy", "baselines/go-caddy.toml", CADDY_REASONS);
}
