//! Resolver acceptance for Python against the django corpus: nothing is
//! dropped, and the measured counts hold the committed baseline.
//!
//! Distinct from `tests/python_corpus.rs`, which is the *extractor*'s
//! acceptance — that one asserts the record-level invariants of one file at a
//! time and never resolves anything. This one runs the whole track and asks
//! the two questions a rate is only worth reading if you can answer:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External`, `LocalBinding` or `Unresolved(reason)`.
//!    The check re-extracts the same files independently and compares totals,
//!    because a resolver that silently dropped its hardest references would
//!    otherwise report a *better* rate for doing less work.
//! 2. **The ratchet.** The counts are compared against `baselines/`
//!    through the same [`arthron::gate::evaluate`] the `arthron gate` command
//!    uses, so a rate regression, or drift in either of the two buckets that
//!    sit outside the rate, fails the build.
//!
//! # Why this is a test and not `arthron gate`
//!
//! It should be `arthron gate corpus/python/django --baseline …`, and today it
//! cannot be, for two reasons in files this track does not own:
//!
//! - `pipeline::scan_repo` runs every live track unconditionally, and the Go
//!   resolver's `config` reads `root/go.mod` before it looks at whether the
//!   walk found any Go at all. A repository that is only Python therefore
//!   fails the whole scan with `reading go.mod: No such file or directory`.
//! - `main.rs` hardcodes `Lang::Go` on both sides of the gate — the baseline's
//!   `language` check and the tally it measures — so there is no way to ask it
//!   for another language's numbers.
//!
//! Both are recorded as core gaps. Until they close, this test is Python's
//! gate: it drives the track's own entry point, the one `scan_repo` would
//! call, and writes its baseline with the product's own renderer so the file
//! is byte-identical to one `--rebase` would produce.
//!
//! Re-base deliberately, exactly as the ratchet requires, with:
//!
//! ```text
//! ARTHRON_PYTHON_REBASE=1 ARTHRON_PYTHON_COMMIT=<sha> \
//!     cargo test --release --test corpus_python
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken resolver.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Baseline, Counts, GateVerdict, evaluate, parse_baseline, render_baseline};
use arthron::model::{DefKind, Lang, node_id, reason_name};
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_python::extract::extract;
use arthron::track_python::resolve::scan_python;

mod support;

const CORPUS: &str = "corpus/python/django";
const BASELINE: &str = "baselines/python-django.toml";

/// Every unresolved reason django produces, exactly.
///
/// Nine buckets, and the spread is the point: Python's resolver reaches
/// further than any other tier-1 track and each of these names a different
/// thing it could not do. `NeedsTypeInference` is the bulk — a receiver whose
/// type is not stated — and the small ones are the interesting ones, because
/// a floor cannot see them at all. Moving the MRO miss out of
/// `NoMatchingDefinition` and into `Generated` relabels 19 references from
/// "not found" to "the target is generated code" and moves nothing this file
/// otherwise gates.
const DJANGO_REASONS: &[(&str, u64)] = &[
    ("DynamicDispatch", 87),
    ("Generated", 13),
    ("NeedsExpressionType", 1609),
    ("NeedsReceiverType", 136),
    ("NeedsTypeInference", 10256),
    ("NoMatchingDefinition", 294),
    ("ProjectLayoutUnknown", 1),
    ("UnindexedSupertype", 1209),
    ("UnknownPackage", 159),
];

/// flask's, exactly. Ten buckets rather than nine — flask imports modules this
/// tree does not vendor, so `ModuleNotFound` is real here and empty on django.
const FLASK_REASONS: &[(&str, u64)] = &[
    ("DynamicDispatch", 1),
    ("Generated", 88),
    ("ModuleNotFound", 12),
    ("NeedsExpressionType", 143),
    ("NeedsReceiverType", 5),
    ("NeedsTypeInference", 2119),
    ("NoMatchingDefinition", 71),
    ("ProjectLayoutUnknown", 33),
    ("UnindexedSupertype", 156),
    ("UnknownPackage", 219),
];

const FLASK: &str = "corpus/python/flask";
const FLASK_BASELINE: &str = "baselines/python-flask.toml";

// -- the definition census -------------------------------------------------
//
// The two questions above are both about references, and neither can see a
// definition go missing: deleting the rule that emits `DefKind::Method`
// removes 7141 nodes from django and moves no bucket, because a call that
// named one of them merely changes *reason* and the reasons are not pinned
// here. `tests/python_corpus.rs` walks the same trees on the extractor side
// and asserts `defs > 0`, which 7141 fewer definitions also satisfies. The
// census below is the assertion that does not.

/// The measurement one Python corpus's census is.
struct Census {
    files: usize,
    defs: &'static [(DefKind, u64)],
    stored: &'static [(DefKind, u64)],
    packages: u64,
    externals: u64,
    pinned: &'static [(&'static str, NodeKind, &'static str, u32)],
}

/// django: 899 files, flat layout, and the largest tree the suite scans.
const DJANGO: Census = Census {
    files: 899,
    // `Module` is one per file. `Alias` is the largest bucket after the
    // methods and is not noise: `from x import y` at module scope is a
    // binding this package exports under its own name, and re-export chains
    // are most of what a Python resolver walks.
    defs: &[
        (DefKind::Function, 1240),
        (DefKind::Method, 7141),
        (DefKind::Type, 1956),
        (DefKind::Var, 2173),
        (DefKind::Field, 5952),
        (DefKind::Property, 776),
        (DefKind::Module, 899),
        (DefKind::Alias, 5963),
    ],
    // Lower on every kind that a class body can restate: `self.x = …`
    // written in two methods of one class is one field, and a name rebound
    // under `if TYPE_CHECKING:` is one alias. `Module` is absent because a
    // module is filed as a package node, counted below.
    stored: &[
        (DefKind::Function, 1232),
        (DefKind::Method, 7138),
        (DefKind::Type, 1951),
        (DefKind::Var, 2127),
        (DefKind::Field, 5304),
        (DefKind::Property, 727),
        (DefKind::Alias, 5931),
    ],
    // One per file: a Python module *is* a file, which is why this equals
    // the file count and Go's does not.
    packages: 899,
    externals: 105,
    pinned: &[
        (
            "django.db.models.base#Model",
            NodeKind::Definition(DefKind::Type),
            "django/db/models/base.py",
            481,
        ),
        (
            "django.db.models.base#Model.save",
            NodeKind::Definition(DefKind::Method),
            "django/db/models/base.py",
            811,
        ),
        // An attribute assigned in a method body, filed under the class and
        // not the method — the owner frame walked to the bottom.
        (
            "django.db.models.base#Model._order",
            NodeKind::Definition(DefKind::Field),
            "django/db/models/base.py",
            1133,
        ),
        // A `@property` on the metaclass: an accessor, not a method, and
        // not a field either.
        (
            "django.db.models.base#ModelBase._base_manager",
            NodeKind::Definition(DefKind::Property),
            "django/db/models/base.py",
            453,
        ),
        (
            "django.core.checks.registry",
            NodeKind::Package,
            "django/core/checks/registry.py",
            1,
        ),
    ],
};

/// flask: 65 files, `src/` layout — the package lives at `src/flask` and
/// nothing named `flask` exists at the root, so every identity below is
/// rooted at the path the manifest points to and not at a guess.
const FLASK_CENSUS: Census = Census {
    files: 65,
    defs: &[
        (DefKind::Function, 459),
        (DefKind::Method, 357),
        (DefKind::Type, 64),
        (DefKind::Var, 108),
        (DefKind::Field, 234),
        (DefKind::Property, 24),
        (DefKind::Module, 65),
        (DefKind::Alias, 557),
    ],
    stored: &[
        (DefKind::Function, 459),
        (DefKind::Method, 343),
        (DefKind::Type, 64),
        (DefKind::Var, 106),
        (DefKind::Field, 211),
        (DefKind::Property, 15),
        (DefKind::Alias, 557),
    ],
    packages: 65,
    externals: 49,
    pinned: &[
        // `src/flask.app`, not `flask.app`: the container is the path from
        // the root, and reading the manifest is what makes an import of
        // `flask.app` reach it.
        (
            "src/flask.app#Flask",
            NodeKind::Definition(DefKind::Type),
            "src/flask/app.py",
            81,
        ),
        (
            "src/flask.app#Flask.__init__",
            NodeKind::Definition(DefKind::Method),
            "src/flask/app.py",
            226,
        ),
        (
            "src/flask.app#Flask.cli",
            NodeKind::Definition(DefKind::Field),
            "src/flask/app.py",
            256,
        ),
        // The re-export in `__init__.py`: an alias, and the identity most
        // imports of flask actually name.
        (
            "src/flask#Flask",
            NodeKind::Definition(DefKind::Alias),
            "src/flask/__init__.py",
            6,
        ),
        ("src/flask.app", NodeKind::Package, "src/flask/app.py", 1),
    ],
};

/// Count the definitions on both sides of the store and compare them with
/// what this corpus's [`Census`] records.
fn assert_census(corpus: &str, census: &Census) {
    let root = Path::new(corpus);
    if !root.is_dir() {
        support::missing(root);
        return;
    }
    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    scan_python(root, &db).expect("the corpus scans");

    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    drop(store);
    assert_eq!(
        owned.len(),
        census.files,
        "{corpus}: the scan owned a different file set",
    );

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
        let id = node_id(Lang::Python.domain(), fqn);
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
fn the_django_definition_census_is_exact() {
    assert_census(CORPUS, &DJANGO);
}

#[test]
fn the_flask_definition_census_is_exact() {
    assert_census(FLASK, &FLASK_CENSUS);
}

#[test]
fn the_python_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        support::missing(corpus);
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_python(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Python.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "python       resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    for (code, count) in &tally.unresolved {
        println!("             {} {count}", reason_name(*code));
    }

    // -- the reasons -------------------------------------------------------

    // Python had no reason assertion at all — not even the `Unknown` and
    // zero-count pair Go and Java carried — while the tier-2 tracks pinned
    // theirs exactly. Nothing else in this file can see a reason: the four
    // numbers the baseline holds are identical whichever one each unresolved
    // reference carries.
    support::assert_reasons(CORPUS, &tally.unresolved, DJANGO_REASONS);

    // -- completeness -----------------------------------------------------

    // Independently re-extracted: the same files the scan owned, read again
    // from disk and put through the extractor with no resolver in sight. The
    // scan's four buckets must account for every one of those references and
    // for nothing else.
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

    // The four buckets are a partition, so none of them may be the whole of
    // it: a run where everything landed in one bucket accounts for every
    // reference and still measures nothing.
    assert!(measured.resolved > 0, "nothing linked at all");
    assert!(measured.unresolved > 0, "no floor: every reason is empty");
    assert!(
        measured.external > 0,
        "nothing reached outside the repository"
    );
    assert!(
        measured.local_binding > 0,
        "no local binding was recognised"
    );

    // -- the ratchet ------------------------------------------------------

    let baseline_path = Path::new(BASELINE);
    if std::env::var_os("ARTHRON_PYTHON_REBASE").is_some() {
        let previous = std::fs::read_to_string(baseline_path)
            .ok()
            .and_then(|text| parse_baseline(&text).ok());
        let baseline = Baseline {
            format: 1,
            corpus: CORPUS.to_string(),
            // `--commit`'s stand-in: provenance, printed and never verified,
            // so it is carried forward rather than invented when unset.
            commit: std::env::var("ARTHRON_PYTHON_COMMIT")
                .ok()
                .filter(|c| !c.is_empty())
                .or_else(|| previous.map(|b| b.commit))
                .unwrap_or_else(|| "unknown".to_string()),
            language: Lang::Python.name().to_string(),
            counts: measured,
        };
        std::fs::write(baseline_path, render_baseline(&baseline))
            .unwrap_or_else(|e| panic!("writing {BASELINE}: {e}"));
        println!("REBASED {BASELINE}");
        return;
    }

    let text = std::fs::read_to_string(baseline_path).unwrap_or_else(|e| {
        panic!("reading {BASELINE}: {e}; record it with ARTHRON_PYTHON_REBASE=1")
    });
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Python.name(),
        "{BASELINE} measures another language; rates are per language and never aggregated",
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

/// The second Python corpus, and the ratchet that holds it.
///
/// flask answers the one question the Python track exists to answer, from the
/// other side: django is a **flat layout**, where the import name `django` is
/// a top-level directory and a resolver that assumes `<root>/<package>` is
/// right by accident; flask is a **`src/` layout**, where the importable
/// package lives at `src/flask` and nothing named `flask` exists at the root.
/// The path from `pyproject.toml` to the package has to be read, not guessed.
///
/// Its baseline is written by the product's own command, which since #10 takes
/// `--language` and no longer hardcodes Go:
///
/// ```text
/// arthron gate corpus/python/flask --language python \
///     --baseline baselines/python-flask.toml --rebase --commit 22d9247
/// ```
///
/// django's baseline above predates that and is still re-based through
/// `ARTHRON_PYTHON_REBASE`; both files are the same format and are compared by
/// the same [`evaluate`], so the difference is in how they are *written*, not
/// in what holds them.
#[test]
fn the_flask_ratchet_holds() {
    let corpus = Path::new(FLASK);
    if !corpus.is_dir() {
        support::missing(corpus);
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let report = scan_python(corpus, &scratch.path().join("graph.redb")).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Python.code())
        .cloned()
        .unwrap_or_default();
    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "flask        resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    for (code, count) in &tally.unresolved {
        println!("             {} {count}", reason_name(*code));
    }
    support::assert_reasons(FLASK, &tally.unresolved, FLASK_REASONS);

    // The four buckets are a partition, so none of them may be the whole of
    // it: a run where everything landed in one bucket accounts for every
    // reference and still measures nothing.
    assert!(measured.resolved > 0, "nothing linked at all");
    assert!(measured.unresolved > 0, "no floor: every reason is empty");
    assert!(
        measured.external > 0,
        "nothing reached outside the repository"
    );

    let text = std::fs::read_to_string(FLASK_BASELINE)
        .unwrap_or_else(|e| panic!("reading {FLASK_BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{FLASK_BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Python.name(),
        "{FLASK_BASELINE} measures another language; rates are per language and never aggregated",
    );
    assert_eq!(
        baseline.corpus, FLASK,
        "{FLASK_BASELINE} was recorded from another corpus",
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
