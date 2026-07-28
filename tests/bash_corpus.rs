//! Acceptance for the Bash track against the bats-core corpus: nothing is
//! dropped, and the measured counts are the ones the committed baseline was
//! recorded from.
//!
//! Three questions; the first and the last are the two every tier-1 corpus
//! test asks, and the middle one is the half of tier 2 no rate reaches:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** Tier 2's deliverable is definitions, structure and
//!    imports, and here the rate can see almost none of the imports — so the
//!    definition census is nearly the whole of what this track delivers, and
//!    it is asserted exactly on both sides of the store. An owner-frame bug
//!    that lost most of the corpus's functions moves no rate, no bucket and
//!    no baseline, so nothing else here would notice it.
//! 3. **The ratchet.** The counts are compared against
//!    `baselines/bash-bats-core.toml` through the same
//!    [`arthron::gate::evaluate`] the `arthron gate` command uses, so a rate
//!    regression — or drift in either of the two buckets that sit outside the
//!    rate — fails the build.
//!
//! # The sanity check this file exists to make explicit
//!
//! **The rate is 0.0%, over a denominator of 6, and that is the honest
//! result.** bats-core was vendored because not one of its `source` targets
//! is a literal path; the corpus provenance records 31 such lines across the
//! whole snapshot with 13 distinct targets and zero constants. Twelve of the
//! forty-five shell files carry an extension this track owns, and those
//! twelve hold six `source` clauses — three composing
//! `"$BATS_ROOT/$BATS_LIBDIR/bats-core/<name>.bash"` out of two run-time
//! variables, and three that are pure run-time values.
//!
//! The tail of each composed path really does name a file in this tree.
//! Matching on it would take this number from 0% to 50% in one commit and
//! would be a guess about variables the running program computes. A small
//! honest denominator with the right reason on every miss is the deliverable;
//! the 91-function census below is what the track is actually for.
//!
//! One consequence is worth stating where it can be seen: with nothing
//! resolved here, the only branch of the resolver that can return `Resolved`
//! is exercised by **zero** rows in this file, so a regression in it would
//! move no count below and would pass the ratchet. `tests/bash_fixture.rs`
//! is where that branch is covered, over a tree written by the test rather
//! than a vendored corpus.
//!
//! **`load` contributes nothing, and that is not a gap.** It is not shell
//! syntax — bats defines it as a function in
//! `lib/bats-core/test_functions.bash` — and its call sites are all in
//! `.bats` files, which this track deliberately does not own. So it appears
//! in no tally, produces no miss, and carries no reason.
//!
//! bats-core is pinned and is never edited, so every number below is a fact
//! about this extractor and this resolver reading a fixed 12 files; a change
//! to any of them is a change in what the track *does*, and must arrive as a
//! deliberate edit here and a deliberate `--rebase` beside it, never as a
//! test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/bash/bats-core --language bash \
//!     --baseline baselines/bash-bats-core.toml --rebase --commit eb7f42f
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
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_bash::extract::{SourceForm, extract};
use arthron::track_bash::resolve::scan_bash;

const CORPUS: &str = "corpus/bash/bats-core";
const BASELINE: &str = "baselines/bash-bats-core.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
///
/// Twelve files out of the snapshot's forty-five shell files: the two `.sh`
/// installers and the ten `.bash` libraries. The other thirty-three are
/// twenty-one `.bats` files and twelve extensionless scripts, neither of
/// which any language claims — see [`the_corpus_surface_is_the_owned_extensions_only`].
const FILES: usize = 12;
const REFERENCES: u64 = 6;
const LITERAL: u64 = 0;
const DYNAMIC: u64 = 6;

/// Every definition the extractor emits over those 12 files, by kind.
///
/// Asserted exactly, because it is very nearly the whole of what this track
/// delivers: the import rate can see six references and this census can see
/// ninety-one functions, so an extractor bug that lost the functions would
/// leave every rate, every bucket and the whole ratchet untouched.
/// `Module` counts the 12 synthetic script nodes; bash has no module of its
/// own.
const DEFS: &[(DefKind, u64)] = &[(DefKind::Function, 91), (DefKind::Module, 12)];

/// Definition nodes the store holds after merging, by kind.
///
/// Equal to [`DEFS`]' function count here, and the pair of censuses is the
/// point: the extractor's says nothing was lost on the way in, the store's
/// says nothing was lost or over-merged on the way through. A function's FQN
/// carries the file that writes it, so two files declaring one name are two
/// nodes — which is why this number does not fall below the extractor's.
///
/// `DefKind::Module` is absent because the driver files a module as a
/// *package* node rather than a definition; those are counted by
/// [`PACKAGES`] instead.
const STORED: &[(DefKind, u64)] = &[(DefKind::Function, 91)];

/// Package nodes: the 12 script nodes a `source` would name, one per owned
/// file. There is nothing else in this domain that is a container.
const PACKAGES: u64 = 12;

/// External nodes. Bash has no manifest, so no repository declares that a
/// name comes from outside it and this track mints none — which is what makes
/// its rate un-gameable by reclassification, since `External` sits outside
/// both rate terms.
const EXTERNALS: u64 = 0;

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. `run.bats_run_print_output`
/// cannot be right unless the enclosing-function frames were walked, and
/// `single-use-latch::wait` cannot be right unless a function name that is
/// not a POSIX name is read as one.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // The script a `source "$BATS_ROOT/$BATS_LIBDIR/bats-core/common.bash"`
    // would name if it named anything statically — the node the six
    // unresolved references are unresolved *against*.
    (
        "$lib/bats-core/common.bash",
        NodeKind::Package,
        "lib/bats-core/common.bash",
        1,
    ),
    (
        "$lib/bats-core/common.bash#bats_trim",
        NodeKind::Definition(DefKind::Function),
        "lib/bats-core/common.bash",
        210,
    ),
    // The one nested function in the corpus: written inside `run`, and its
    // identity says so.
    (
        "$lib/bats-core/test_functions.bash#run",
        NodeKind::Definition(DefKind::Function),
        "lib/bats-core/test_functions.bash",
        310,
    ),
    (
        "$lib/bats-core/test_functions.bash#run.bats_run_print_output",
        NodeKind::Definition(DefKind::Function),
        "lib/bats-core/test_functions.bash",
        417,
    ),
    // A `.sh` file, so the census covers both owned extensions and not just
    // the one the libraries use.
    (
        "$uninstall.sh#remove_file",
        NodeKind::Definition(DefKind::Function),
        "uninstall.sh",
        27,
    ),
    // Function names bash allows and a POSIX `name` does not: `-` and `::`.
    (
        "$test/concurrent-coordination.bash#single-use-barrier",
        NodeKind::Definition(DefKind::Function),
        "test/concurrent-coordination.bash",
        4,
    ),
    (
        "$test/concurrent-coordination.bash#single-use-latch::wait",
        NodeKind::Definition(DefKind::Function),
        "test/concurrent-coordination.bash",
        32,
    ),
    // A function declared on the file's first line, which is where an
    // off-by-one in the span would show.
    (
        "$test/test_helper.bash#emulate_bats_env",
        NodeKind::Definition(DefKind::Function),
        "test/test_helper.bash",
        1,
    ),
    (
        "$lib/bats-core/validator.bash#bats_test_count_validator",
        NodeKind::Definition(DefKind::Function),
        "lib/bats-core/validator.bash",
        3,
    ),
];

/// Every `source` clause in the owned files, spelled out:
/// `(file, line, raw target)`.
///
/// The whole denominator, named. Six is small enough to write down, and
/// writing it down is what makes "the rate is zero" a statement about this
/// corpus rather than about this extractor having stopped emitting.
const CLAUSES: &[(&str, u32, &str)] = &[
    (
        "lib/bats-core/preprocessing.bash",
        18,
        "source \"${BATS_TEST_SOURCE?}\"",
    ),
    (
        "lib/bats-core/test_functions.bash",
        12,
        "source \"$BATS_ROOT/$BATS_LIBDIR/bats-core/warnings.bash\"",
    ),
    (
        "lib/bats-core/test_functions.bash",
        67,
        "source \"$library_load_path\"",
    ),
    ("lib/bats-core/test_functions.bash", 108, "source \"$1\""),
    (
        "lib/bats-core/tracing.bash",
        4,
        "source \"$BATS_ROOT/$BATS_LIBDIR/bats-core/common.bash\"",
    ),
    (
        "lib/bats-core/warnings.bash",
        4,
        "source \"$BATS_ROOT/$BATS_LIBDIR/bats-core/tracing.bash\"",
    ),
];

#[test]
fn the_corpus_surface_is_the_owned_extensions_only() {
    // Nothing in this assertion needs the corpus, and that is the point: the
    // claim is about what the language partition says, and it holds whether
    // or not the snapshot is on disk.
    assert_eq!(Lang::Bash.extensions(), ["sh", "bash"]);
    // A `.bats` file parses under the shell grammar without complaint and is
    // misread; twenty-one of the corpus's forty-five shell files are `.bats`.
    assert_eq!(Lang::for_extension("bats"), None);
    // Twelve more are extensionless — `bin/bats` and `libexec/bats-core/*` —
    // and ownership here is by extension, so the walk never offers them.
    assert_eq!(Lang::for_extension(""), None);
}

#[test]
fn the_bash_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_bash(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Bash.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "bash         resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    for (code, count) in &tally.unresolved {
        println!("             {} {count}", reason_name(*code));
        reasons.insert(reason_name(*code).to_string(), *count);
    }
    // A file the walk could not read is a file whose definitions are missing
    // from the census below, so it is never allowed to pass silently.
    assert!(report.file_errors.is_empty(), "{:?}", report.file_errors);

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
            rel.ends_with(".sh") || rel.ends_with(".bash"),
            "{rel} carries an extension this track does not own",
        );
    }

    let mut re_extracted = 0u64;
    let mut forms: BTreeMap<&str, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    let mut clauses: Vec<(String, u32, String)> = Vec::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        let facts = extract(rel, &source);
        re_extracted += facts.refs.len() as u64;
        for r in &facts.refs {
            // The tier-2 contract, checked on real code and not only on a
            // fixture. It bites hardest here: a bash call site is an ordinary
            // `command`, so a `Call` in this list would mean the denominator
            // had become a count of shell commands.
            assert_eq!(r.kind, RefKind::Import, "{rel}: {}", r.raw_target);
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
            clauses.push((rel.clone(), r.span.line, r.raw_target.clone()));
        }
        // A clause and its reference are paired by span, so a clause with no
        // reference would be a silently dropped import.
        assert_eq!(
            facts.header.sources.len(),
            facts.refs.len(),
            "{rel}: source clauses and import references disagree",
        );
        for spec in &facts.header.sources {
            *forms
                .entry(match spec.form {
                    SourceForm::Literal(_) => "literal",
                    SourceForm::Dynamic => "dynamic",
                })
                .or_default() += 1;
        }
        // Every owned file declares the script a `source` names, first,
        // whether or not it declares a function.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no script",
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
        "the definition census moved; with a six-reference denominator the \
         census is very nearly all this track delivers and no rate can see it",
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
    assert_eq!(forms.get("literal").copied().unwrap_or(0), LITERAL);
    assert_eq!(forms.get("dynamic").copied(), Some(DYNAMIC));

    // Not one `source` target in this corpus is a literal path, so nothing
    // links. Stated as a number rather than left to be inferred from the
    // reason histogram.
    assert_eq!(measured.resolved, 0);
    assert_eq!(measured.external, 0, "this track mints no external node");
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The bucket that sits outside both rate terms is empty, which is
    // what makes this rate un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    assert_eq!(measured.unresolved, 6);

    // One reason, and it is the honest one: `$BATS_ROOT` is derived at run
    // time from the resolved path of `$0` and `$BATS_LIBDIR` is an
    // environment variable with a default, so the tail of a composed path is
    // not a target that was named.
    assert_eq!(reasons.get("DynamicModuleSpecifier").copied(), Some(6));
    assert_eq!(
        reasons.len(),
        1,
        "an unexpected reason appeared: {reasons:?}"
    );

    // The denominator, written out. A rate of zero over six is a fact about
    // this corpus; a rate of zero over nothing at all would be a broken
    // extractor, and only the sites themselves tell the two apart.
    clauses.sort();
    let want: Vec<(String, u32, String)> = CLAUSES
        .iter()
        .map(|(f, l, t)| ((*f).to_string(), *l, (*t).to_string()))
        .collect();
    let mut want_sorted = want.clone();
    want_sorted.sort();
    assert_eq!(clauses, want_sorted, "the `source` sites moved");

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
    // Nothing merged: every function in this corpus has its own identity,
    // and the two censuses agreeing is what says so.
    assert_eq!(
        stored.get(&DefKind::Function.code()).copied(),
        kinds.get(&DefKind::Function.code()).copied(),
    );

    for (fqn, kind, file, line) in PINNED {
        let id = node_id(Domain::Shell, fqn);
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
    drop(read);

    // -- the ratchet ------------------------------------------------------

    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Bash.name(),
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
