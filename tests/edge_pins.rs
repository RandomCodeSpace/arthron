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
//! - a row that **vanished** with nothing in its place is flagged in the
//!   output, not failed — the counting gate (`denominator_shrank`, `external`,
//!   `local_binding`) and the deleted lines in this file's own git diff are
//!   what carry that half;
//! - a row that vanished **while another appeared** is a row whose key
//!   changed, and that fails by name — `rows_rekeyed`. The hand-off above is
//!   only good for vanished rows nothing replaced: re-keying preserves all
//!   four gated integers by construction, so no other check in the build would
//!   ever say those rows stopped being compared.
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

/// The converse of the test above, which only refuses an orphan pin file.
///
/// `tests/baselines.rs` has both halves — `every_committed_baseline_is_gated_
/// by_a_test` and `every_gated_baseline_has_a_step_in_the_corpus_gate_
/// workflow` — and pins had only one, so a new tier-1 corpus could land with a
/// baseline, a ratchet and a workflow step and no pin file at all. Nothing
/// would say so: the four gated integers would be measured and the wrong-edge
/// surface this file exists for would simply be unguarded on that corpus.
///
/// Both sides are already programmatic, so this needs no second table: every
/// baseline records its `corpus` and its `language`, and [`Lang::tier`] is
/// what says which languages build the call, type-use and field-access graph
/// that a pin file is for.
#[test]
fn every_tier_1_corpus_with_a_baseline_has_a_pin_file() {
    use arthron::gate::parse_baseline;
    use arthron::model::Lang;

    let pinned: BTreeSet<&str> = PINNED.iter().map(|(_, corpus)| *corpus).collect();
    let mut checked = 0;
    for entry in std::fs::read_dir("baselines").expect("baselines/ is committed") {
        let path = entry.expect("a directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let shown = path.to_string_lossy().replace('\\', "/");
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {shown}: {e}"));
        let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{shown}: {e}"));
        let lang = Lang::ALL
            .iter()
            .find(|l| l.name() == baseline.language)
            .unwrap_or_else(|| {
                panic!(
                    "{shown} names language `{}`, which no variant carries",
                    baseline.language,
                )
            });
        if lang.tier() != 1 {
            continue;
        }
        checked += 1;
        assert!(
            pinned.contains(baseline.corpus.as_str()),
            "{shown} gates {}, a tier-1 corpus, and no pin file pins it. A tier-1 \
             track builds the call graph, so a reference there can resolve to the \
             wrong definition without moving any of the four integers that baseline \
             holds — add `pins/<lang>-<corpus>.pins` and a row in PINNED in the same \
             commit as the baseline",
            baseline.corpus,
        );
    }
    assert!(
        checked > 0,
        "this test read no tier-1 baseline at all, so it asserted nothing",
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
    let verdict = compare(&pins, &rows).expect("a rendered pin file names every target it uses");
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

    let verdict = compare(&pins, &wrong).expect("a rendered pin file names every target it uses");
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
    let verdict = compare(&pins, &rows).expect("a rendered pin file names every target it uses");
    assert!(!verdict.failed(), "{}", verdict.report());
    assert_eq!(verdict.appeared, (rows.len() - 2) as u64);
    assert!(verdict.report().contains("appeared"));
}

#[test]
fn a_row_that_vanished_is_flagged_and_does_not_fail() {
    let rows = fixture_rows();
    let pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering"))
        .expect("parsing");
    let verdict =
        compare(&pins, &rows[..1]).expect("a rendered pin file names every target it uses");
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

/// The failure this build's second-worst hole was: an outcome-neutral key
/// change that retires a pin file without failing anything.
///
/// A row's key is its identity here, so touching what the key is made of —
/// `argc`, `enclosing`, `raw_target`, `locally_bound` — re-keys rows wholesale.
/// Every re-keyed row reads as one that vanished and one that appeared, and
/// both of those were legal: the counting gate was said to own the vanished
/// half. It cannot own this one. Re-keying preserves `resolved`, `external`,
/// `local_binding` and `unresolved` *by construction* — the same references
/// resolve to the same places, only the name of the row changed — so
/// `denominator_shrank` and both drift checks stay green while the rows stop
/// being compared at all.
#[test]
fn a_row_that_was_re_keyed_fails_rather_than_retiring_its_own_pin() {
    let rows = fixture_rows();
    let pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering"))
        .expect("parsing");

    // The target does not move. Only the key does — exactly what an extractor
    // edit to the arity rule does to every call site at once.
    let mut rekeyed = rows.clone();
    rekeyed[1].key.argc = Some(9);

    let verdict = compare(&pins, &rekeyed).expect("a rendered pin file names every target");
    assert_eq!(verdict.held, 2);
    assert_eq!(verdict.appeared, 1);
    assert_eq!(verdict.vanished.len(), 1);
    assert!(verdict.moved.is_empty(), "no target moved, and none should");
    assert_eq!(verdict.rekeyed(), 1);
    assert!(
        verdict.failed(),
        "a row re-keyed out of the comparison stops being checked, and nothing else \
         in the build says so:\n{}",
        verdict.report(),
    );
    assert!(
        verdict.report().contains("rows_rekeyed"),
        "{}",
        verdict.report()
    );
}

/// The other side of the same rule, kept intact: a vanished row that nothing
/// replaced is still only flagged, because the counting gate genuinely does
/// own that one — the resolved set got smaller, and `denominator_shrank`
/// refuses that.
#[test]
fn a_vanished_row_with_nothing_in_its_place_is_still_only_flagged() {
    let rows = fixture_rows();
    let pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering"))
        .expect("parsing");
    let verdict = compare(&pins, &rows[..2]).expect("a rendered pin file names every target");
    assert_eq!(verdict.vanished.len(), 1);
    assert_eq!(verdict.appeared, 0);
    assert_eq!(verdict.rekeyed(), 0);
    assert!(!verdict.failed(), "{}", verdict.report());
}

/// Coverage growth on its own is still growth: rows appear, none vanishes,
/// and `rekeyed` is the part of `vanished` that was offset — zero here.
#[test]
fn rows_that_only_appeared_are_not_read_as_a_re_key() {
    let rows = fixture_rows();
    let pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows[..1]).expect("rendering"))
        .expect("parsing");
    let verdict = compare(&pins, &rows).expect("a rendered pin file names every target");
    assert_eq!(verdict.appeared, 2);
    assert!(verdict.vanished.is_empty());
    assert_eq!(verdict.rekeyed(), 0);
    assert!(!verdict.failed(), "{}", verdict.report());
}

/// A shrinking resolved set is handed to the counting gate whole, not netted
/// against whatever growth happened alongside it: only the offset part is
/// re-keying, and the rest is still a smaller denominator.
#[test]
fn only_the_offset_part_of_a_shrink_is_counted_as_a_re_key() {
    let rows = fixture_rows();
    let pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering"))
        .expect("parsing");
    let mut one_left = vec![rows[0].clone()];
    one_left[0].key.argc = Some(7);
    let verdict = compare(&pins, &one_left).expect("a rendered pin file names every target");
    assert_eq!(verdict.vanished.len(), 3);
    assert_eq!(verdict.appeared, 1);
    assert_eq!(verdict.rekeyed(), 1);
    assert!(verdict.failed(), "{}", verdict.report());
}

/// [`render`] refuses to write two rows of one file at one key hash, and
/// [`parse`] has to refuse to read one: the two header counts are documented
/// as the checksum that makes a mangled file unreadable, and a duplicated row
/// balances `rows` exactly as a distinct one does.
#[test]
fn a_pin_file_with_two_rows_at_one_key_is_refused() {
    let rows = fixture_rows();
    let text = render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let row = lines
        .iter()
        .position(|l| l.starts_with('\t'))
        .expect("a rendered file has rows");
    // The same key hash twice under one file, with the header's counts kept
    // honest: three rows become four, so `rows` is bumped to match.
    let dup = lines[row].clone();
    lines.insert(row, dup);
    for line in &mut lines {
        if *line == format!("rows = {}", rows.len()) {
            *line = format!("rows = {}", rows.len() + 1);
        }
    }
    let bent = lines.join("\n") + "\n";
    let err = parse(&bent).expect_err("two rows at one key make the file ambiguous");
    assert!(err.contains("share the key hash"), "{err}");
}

/// [`compare`] takes a [`Pins`] by reference and every field of it is public,
/// so a caller can hand it one [`parse`] would have refused. Answering that
/// with an out-of-bounds index is this module deciding a caller's error is
/// fatal; it reports instead.
#[test]
fn compare_reports_a_target_index_no_dictionary_entry_carries() {
    let rows = fixture_rows();
    let mut pins = parse(&render("corpus/x", "abc1234", "pins/x.pins", &rows).expect("rendering"))
        .expect("parsing");
    let beyond = pins.targets.len() as u32;
    for entries in pins.files.values_mut() {
        for entry in entries.iter_mut() {
            entry.1 = beyond;
        }
    }
    let err = compare(&pins, &rows).expect_err("a target index the dictionary does not hold");
    assert!(err.contains("dictionary holds"), "{err}");
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
    let verdict = compare(&pins, &rows).expect("a rendered pin file names every target it uses");
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
// The command itself. Still no corpus: a three-file Go tree in a tempdir.
// ---------------------------------------------------------------------------

/// A tree that resolves exactly one call, written under `root`.
fn tiny_corpus(root: &Path) {
    std::fs::write(root.join("go.mod"), "module example.com/app\n\ngo 1.22\n")
        .expect("writing go.mod");
    std::fs::write(
        root.join("app.go"),
        "package app\n\nfunc Caller() {\n\thelper()\n}\n\nfunc helper() {}\n",
    )
    .expect("writing app.go");
}

fn arthron(args: &[&std::ffi::OsStr]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args(args)
        .output()
        .expect("running the built binary")
}

/// `--write` refuses a scan that resolved nothing, and so does the comparison.
///
/// The two were not symmetric: the write path refused, and the compare path
/// read every pinned row as vanished with nothing appearing in its place —
/// flagged, not failed — and exited 0. That is a green run over an empty tree,
/// which is the one thing a pin check must never be.
#[test]
fn a_comparison_over_a_tree_that_resolves_nothing_is_refused() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path().join("corpus");
    std::fs::create_dir_all(&root).expect("the corpus root");
    tiny_corpus(&root);
    let pins = dir.path().join("x.pins");

    let wrote = arthron(&[
        "pin".as_ref(),
        root.as_os_str(),
        "--pins".as_ref(),
        pins.as_os_str(),
        "--write".as_ref(),
    ]);
    assert!(
        wrote.status.success(),
        "writing the pin file: {}",
        String::from_utf8_lossy(&wrote.stderr),
    );

    // Same tree, same path — so the provenance check below is not what fires —
    // and nothing left in it to resolve.
    std::fs::remove_file(root.join("app.go")).expect("emptying the tree");
    let out = arthron(&[
        "pin".as_ref(),
        root.as_os_str(),
        "--pins".as_ref(),
        pins.as_os_str(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a scan that resolved nothing agrees with nothing:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("resolved no reference at all"), "{stderr}");
}

/// A pin file compared against a tree it was not taken over is refused.
///
/// `corpus` is documented as provenance the parser does not verify, and it was
/// verified nowhere else either: a typo in a CI line, or a corpus renamed
/// under a pin file that kept its old name, joins on nothing at all — every
/// pinned row vanishes, every scanned row appears, and the run is green while
/// checking no edge.
#[test]
fn a_pin_file_compared_against_another_tree_is_refused() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let mine = dir.path().join("mine");
    let theirs = dir.path().join("theirs");
    for root in [&mine, &theirs] {
        std::fs::create_dir_all(root).expect("a corpus root");
        tiny_corpus(root);
    }
    let pins = dir.path().join("mine.pins");

    let wrote = arthron(&[
        "pin".as_ref(),
        mine.as_os_str(),
        "--pins".as_ref(),
        pins.as_os_str(),
        "--write".as_ref(),
    ]);
    assert!(
        wrote.status.success(),
        "writing the pin file: {}",
        String::from_utf8_lossy(&wrote.stderr),
    );

    // Byte-identical trees, so the rows would in fact all hold: the refusal is
    // about provenance, not about disagreement.
    let out = arthron(&[
        "pin".as_ref(),
        theirs.as_os_str(),
        "--pins".as_ref(),
        pins.as_os_str(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a pin file must not be compared against a tree it does not name:\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    for needle in [
        mine.display().to_string(),
        theirs.display().to_string(),
        "checks no edge at all".to_string(),
    ] {
        assert!(
            stderr.contains(&needle),
            "the refusal omits {needle:?}: {stderr}"
        );
    }

    // And the tree it does name still passes, so the check is not simply
    // refusing everything.
    let out = arthron(&[
        "pin".as_ref(),
        mine.as_os_str(),
        "--pins".as_ref(),
        pins.as_os_str(),
    ]);
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
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

    let verdict = compare(&pins, &rows).unwrap_or_else(|e| panic!("{pin_path}: {e}"));
    println!("{pin_path}\n{}", verdict.report());
    assert!(
        !verdict.failed(),
        "{pin_path}: a pinned edge is not where it was pinned — a target moved, a \
         pinned row was re-keyed out of the comparison, or two rows share one key. \
         None of those moves the four gated integers, which is why this check \
         exists. Re-pin only with every change attributed:\n{}",
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
