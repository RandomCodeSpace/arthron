//! The target-stability gate: every resolved edge's target, pinned by name.
//!
//! The primary gate compares four integers — `resolved`, `unresolved`,
//! `external`, `local_binding` — so a reference that resolves to the **wrong**
//! definition moves none of them. It is still `Resolved`, still one row, still
//! one edge; only the far end changed. The rate cannot see it, the
//! `denominator_shrank` check cannot see it, and the two drift checks cannot
//! see it, because none of them reads a target. That is the failure mode a
//! type environment introduces at scale, and this file is what makes it fail a
//! build.
//!
//! The rule, from the ratified design (AM-2):
//!
//! - a pinned row whose target **changed** fails, by name — `target_moved`;
//! - a row that **appeared** is legal, and is coverage growth;
//! - a row that **vanished** is flagged in the output, not failed — the
//!   counting gate (`denominator_shrank`, `external`, `local_binding`) and the
//!   deleted lines in this file's own git diff are what carry that half.
//!
//! Two halves, like `tests/baselines.rs`: the pin files themselves are checked
//! everywhere, corpus or no corpus, so a pin file nothing compares against
//! cannot land; the comparison itself needs the corpus and skips without it.
//!
//! No workflow step is added for it, and none is needed. `.github/workflows/
//! gate.yml` already ends by running the whole suite with
//! `ARTHRON_REQUIRE_CORPUS=1` in the one job that fetches the corpus, which is
//! where a skip becomes a failure — so these eleven comparisons run there and
//! block a merge on the same terms every census already does. Eleven cold
//! scans, 12.7 s wall on the reference measurement, against a 45-minute job.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use arthron::config::Config;
use arthron::pins::{FORMAT, collect, compare, parse, render, row_hash};
use arthron::pipeline::scan_repo_with;
use arthron::store::ReadStore;

mod support;

/// Every committed pin file and the corpus it pins.
///
/// Tier 1 only. A tier-2 track emits `Import` references and nothing else, so
/// its resolved set is a handful of module paths that the import ratchet
/// already states exactly; the wrong-edge surface this file exists for is the
/// call, type-use and field-access graph, which only the tier-1 tracks build.
const PINNED: &[(&str, &str)] = &[
    ("pins/go-codeiq.pins", "corpus/go/codeiq"),
    ("pins/go-caddy.pins", "corpus/go/caddy"),
    ("pins/go-probes.pins", "corpus/go/probes"),
    ("pins/java-commons-lang.pins", "corpus/java/commons-lang"),
    ("pins/java-gson.pins", "corpus/java/gson"),
    ("pins/javascript-express.pins", "corpus/javascript/express"),
    ("pins/javascript-fastify.pins", "corpus/javascript/fastify"),
    ("pins/python-django.pins", "corpus/python/django"),
    ("pins/python-flask.pins", "corpus/python/flask"),
    (
        "pins/typescript-vue-core.pins",
        "corpus/typescript/vue-core",
    ),
    ("pins/typescript-zod.pins", "corpus/typescript/zod"),
];

// ---------------------------------------------------------------------------
// The half that needs no corpus.
// ---------------------------------------------------------------------------

#[test]
fn every_committed_pin_file_is_compared_by_this_test() {
    let mut on_disk = BTreeSet::new();
    for entry in std::fs::read_dir("pins").expect("pins/ is committed") {
        let path = entry.expect("a directory entry").path();
        if path.extension().and_then(|e| e.to_str()) == Some("pins") {
            on_disk.insert(path.to_string_lossy().replace('\\', "/"));
        }
    }
    let listed: BTreeSet<String> = PINNED.iter().map(|(path, _)| (*path).to_string()).collect();
    assert_eq!(
        on_disk, listed,
        "a pin file nothing compares against is the absence of a gate, not a passing one",
    );
}

#[test]
fn every_pin_file_parses_and_names_its_own_corpus() {
    let mut corpora = BTreeSet::new();
    for (path, corpus) in PINNED {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        let pins = parse(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(
            pins.format, FORMAT,
            "{path} is a format this build cannot read"
        );
        assert_eq!(&pins.corpus, corpus, "{path} names the wrong corpus");
        assert!(
            pins.rows() > 0,
            "{path} pins no row at all and would bless any scan"
        );
        assert!(
            corpora.insert(pins.corpus.clone()),
            "{path} pins {corpus}, which another pin file already pins",
        );
    }
}

#[test]
fn the_header_documents_the_one_command_that_regenerates_the_file() {
    for (path, corpus) in PINNED {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        let header: String = text
            .lines()
            .take_while(|l| !l.starts_with('['))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            header.contains(&format!("arthron pin {corpus} --pins {path} --write")),
            "{path}: the header does not carry the command that regenerates it",
        );
    }
}

#[test]
fn a_pin_file_round_trips() {
    let rows = fixture_rows();
    let text = render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering");
    let pins = parse(&text).expect("parsing what render wrote");
    assert_eq!(pins.corpus, "corpus/x");
    assert_eq!(pins.commit, "abc1234");
    assert_eq!(pins.rows(), rows.len() as u64);
    // Rendering is deterministic: the same rows in a different order produce
    // byte-identical output, or a pin file would churn on every run.
    let mut shuffled = rows.clone();
    shuffled.reverse();
    assert_eq!(
        text,
        render("corpus/x", "abc1234", "pins/x.pins", &shuffled).expect("rendering")
    );
}

#[test]
fn a_scan_that_agrees_with_its_pins_holds_every_row() {
    let rows = fixture_rows();
    let pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering"))
        .expect("parsing");
    let verdict = compare(&pins, &rows);
    assert!(!verdict.failed(), "{}", verdict.report());
    assert_eq!(verdict.held, rows.len() as u64);
    assert_eq!(verdict.appeared, 0);
    assert!(verdict.vanished.is_empty());
}

#[test]
fn a_target_that_moved_fails_and_names_the_row() {
    let rows = fixture_rows();
    let pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering"))
        .expect("parsing");

    let mut wrong = rows.clone();
    wrong[1].target = "def x/y.Wrong".to_string();

    let verdict = compare(&pins, &wrong);
    assert!(verdict.failed(), "a moved target must fail the gate");
    assert_eq!(verdict.moved.len(), 1);
    let moved = &verdict.moved[0];
    assert_eq!(moved.file, wrong[1].key.file);
    assert_eq!(moved.raw_target, wrong[1].key.raw_target);
    assert_eq!(moved.line, wrong[1].line);
    assert_eq!(moved.was, rows[1].target);
    assert_eq!(moved.now, "def x/y.Wrong");

    // The report is the whole point: a hash diff is useless, so every one of
    // these has to be in the text a failing build prints.
    let text = verdict.report();
    for needle in [
        "target_moved".to_string(),
        moved.file.clone(),
        moved.raw_target.clone(),
        moved.was.clone(),
        moved.now.clone(),
        moved.line.to_string(),
    ] {
        assert!(
            text.contains(&needle),
            "the report omits {needle:?}:\n{text}"
        );
    }
}

#[test]
fn a_row_that_appeared_is_coverage_and_passes() {
    let rows = fixture_rows();
    let pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows[..2]).expect("rendering"))
        .expect("parsing");
    let verdict = compare(&pins, &rows);
    assert!(!verdict.failed(), "{}", verdict.report());
    assert_eq!(verdict.appeared, (rows.len() - 2) as u64);
    assert!(verdict.report().contains("appeared"));
}

#[test]
fn a_row_that_vanished_is_flagged_and_does_not_fail() {
    let rows = fixture_rows();
    let pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering"))
        .expect("parsing");
    let verdict = compare(&pins, &rows[..1]);
    assert!(
        !verdict.failed(),
        "a vanished row is flagged, not failed — the counting gate owns that half",
    );
    assert_eq!(verdict.vanished.len(), rows.len() - 1);
    let text = verdict.report();
    assert!(text.contains("row_vanished"), "{text}");
    // Flagged means named: the file and the target that left have to be in
    // the output, because that is all a vanished row can still say about
    // itself — there is no current row left to join against.
    assert!(text.contains(&rows[1].target), "{text}");
    assert!(text.contains(&rows[1].key.file), "{text}");
}

#[test]
fn two_rows_of_one_file_that_differ_only_in_arity_are_two_pins() {
    let mut rows = fixture_rows();
    let mut other = rows[0].clone();
    other.key.argc = Some(2);
    other.target = "def x/y.Two".to_string();
    rows.push(other.clone());
    assert_ne!(row_hash(&rows[0].key), row_hash(&other.key));

    let pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering"))
        .expect("parsing");
    let verdict = compare(&pins, &rows);
    assert!(!verdict.failed(), "{}", verdict.report());
    assert_eq!(verdict.held, rows.len() as u64);
}

#[test]
fn a_pin_file_whose_header_disagrees_with_its_body_is_refused() {
    let rows = fixture_rows();
    let text = render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering");
    let bent = text.replace(
        &format!("rows = {}", rows.len()),
        &format!("rows = {}", rows.len() + 1),
    );
    assert_ne!(bent, text, "the header must carry a row count to bend");
    let err = parse(&bent).expect_err("a header that disagrees with the body is not readable");
    assert!(err.contains("rows"), "{err}");
}

/// Three resolved rows, hand-built, in two files.
fn fixture_rows() -> Vec<arthron::pins::ResolvedRow> {
    use arthron::pins::ResolvedRow;
    use arthron::store::RefKey;
    let key = |file: &str, enclosing: &str, raw: &str, argc: Option<u32>| RefKey {
        file: file.to_string(),
        kind: arthron::model::RefKind::Call.code(),
        space: arthron::model::DeclSpace::Value.code(),
        enclosing: enclosing.to_string(),
        raw_target: raw.to_string(),
        argc,
        locally_bound: false,
    };
    vec![
        ResolvedRow {
            key: key("a.go", "x/y.Caller", "helper", Some(1)),
            line: 12,
            target: "def x/y.helper".to_string(),
        },
        ResolvedRow {
            key: key("a.go", "x/y.Other", "helper", Some(1)),
            line: 40,
            target: "def x/y.helper".to_string(),
        },
        ResolvedRow {
            key: key("b.go", "x/y.Third", "Widget", None),
            line: 3,
            target: "def x/y.Widget".to_string(),
        },
    ]
}

// ---------------------------------------------------------------------------
// The half that needs the corpus.
// ---------------------------------------------------------------------------

/// Scan one corpus cold and compare every resolved edge against its pins.
fn check(pin_path: &str, corpus: &str) {
    let corpus = Path::new(corpus);
    if !corpus.is_dir() {
        support::missing(corpus);
        return;
    }
    let text =
        std::fs::read_to_string(pin_path).unwrap_or_else(|e| panic!("reading {pin_path}: {e}"));
    let pins = parse(&text).unwrap_or_else(|e| panic!("{pin_path}: {e}"));

    let dir = tempfile::tempdir().expect("a scratch directory");
    let db: PathBuf = dir.path().join("pins.redb");
    let config = Config::load(corpus).unwrap_or_else(|e| panic!("{}: {e}", corpus.display()));
    scan_repo_with(corpus, &db, &config)
        .unwrap_or_else(|e| panic!("scanning {}: {e}", corpus.display()));
    let store = ReadStore::open(&db).expect("opening the store this scan just wrote");
    let rows = collect(&store).expect("collecting resolved rows");
    // A corpus directory that exists and holds nothing resolves nothing, and
    // every pinned row would read as vanished — which is flagged and does not
    // fail. That is a green run over an empty tree, so it is refused here
    // rather than reported as agreement.
    assert!(
        !rows.is_empty(),
        "{}: this scan resolved no reference at all, so it agrees with nothing",
        corpus.display(),
    );

    let verdict = compare(&pins, &rows);
    println!("{pin_path}\n{}", verdict.report());
    assert!(
        !verdict.failed(),
        "{pin_path}: a resolved reference now points somewhere else. A wrong edge \
         moves none of the four gated integers, which is why this check exists. \
         Re-pin only with every changed edge attributed:\n{}",
        verdict.report(),
    );
}

#[test]
fn go_codeiq_edges_are_where_they_were_pinned() {
    check(PINNED[0].0, PINNED[0].1);
}

#[test]
fn go_caddy_edges_are_where_they_were_pinned() {
    check(PINNED[1].0, PINNED[1].1);
}

#[test]
fn go_probes_edges_are_where_they_were_pinned() {
    check(PINNED[2].0, PINNED[2].1);
}

#[test]
fn java_commons_lang_edges_are_where_they_were_pinned() {
    check(PINNED[3].0, PINNED[3].1);
}

#[test]
fn java_gson_edges_are_where_they_were_pinned() {
    check(PINNED[4].0, PINNED[4].1);
}

#[test]
fn javascript_express_edges_are_where_they_were_pinned() {
    check(PINNED[5].0, PINNED[5].1);
}

#[test]
fn javascript_fastify_edges_are_where_they_were_pinned() {
    check(PINNED[6].0, PINNED[6].1);
}

#[test]
fn python_django_edges_are_where_they_were_pinned() {
    check(PINNED[7].0, PINNED[7].1);
}

#[test]
fn python_flask_edges_are_where_they_were_pinned() {
    check(PINNED[8].0, PINNED[8].1);
}

#[test]
fn typescript_vue_core_edges_are_where_they_were_pinned() {
    check(PINNED[9].0, PINNED[9].1);
}

#[test]
fn typescript_zod_edges_are_where_they_were_pinned() {
    check(PINNED[10].0, PINNED[10].1);
}
