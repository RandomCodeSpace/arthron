//! `arthron` CLI. Printing only — analysis logic lives in the library.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use arthron::config::Config;
use arthron::gate::{
    Baseline, Counts, FORMAT, GateVerdict, evaluate, is_renderable, parse_baseline, render_baseline,
};
use arthron::json;
use arthron::mcp;
use arthron::model::{Lang, reason_name};
use arthron::pipeline::scan_repo_with;
use arthron::query::{
    DEFAULT_IMPACT_DEPTH, Impact, Match, NameIndex, NodeKind, RefSite, definition, impact,
    references,
};
use arthron::registry::REGISTRY;
use arthron::resolution_rate;
use arthron::store::{LangTally, ReadStore, StoredOutcome};

/// Exit code for a gate regression: the run worked, the numbers are worse.
const EXIT_GATE_FAILED: u8 = 1;
/// Exit code for a query that selected no single node — no match, or several.
///
/// Deliberately the same value as [`EXIT_GATE_FAILED`] and deliberately a
/// different constant: both mean "the command ran and the answer is no", and
/// neither means the run failed. A store that would not open is
/// [`EXIT_USAGE`] instead, because then there is no answer at all.
const EXIT_NO_ANSWER: u8 = 1;
/// Exit code for usage and I/O problems: nothing was measured, so neither a
/// pass nor a failure may be reported.
const EXIT_USAGE: u8 = 2;

/// Where a query looks for the graph when `--db` is not given: the path
/// `arthron scan .` writes.
const DEFAULT_DB: &str = ".arthron/graph.redb";

/// `writeln!` into an answer being assembled.
///
/// Every command here builds its whole answer as text and hands it to
/// [`emit`] once, rather than printing as it goes — see [`emit`] for why. The
/// sink is a `String`, whose `fmt::Write` cannot fail; the `Result` the trait
/// must declare is therefore unreachable and is dropped here, in one place,
/// instead of at every call site.
macro_rules! outln {
    ($buf:expr, $($arg:tt)*) => {{
        let _ = std::fmt::Write::write_fmt(&mut $buf, format_args!($($arg)*));
        $buf.push('\n');
    }};
}

/// Write a finished answer to stdout, and return the code the answer carries.
///
/// The reader leaving early — `| head`, `| less` quit, `| grep -q` satisfied,
/// `| jq` exiting — closes the pipe, and the `println!` family answers that by
/// panicking: exit 101 and a backtrace, which no script can tell from a real
/// crash. A closed reader is not a failure of this program, so it is not
/// reported as one; the answer was produced and printed as far as anybody was
/// reading, and the code the answer carries is still the truth about it.
///
/// One write, not many, so there is exactly one place the pipe can be found
/// closed. Any *other* write failure — a full disk — is a genuine I/O failure
/// and stays [`EXIT_USAGE`], because then the answer did not reach anyone and
/// silence would be indistinguishable from success.
fn emit(text: &str, answered: ExitCode) -> ExitCode {
    use std::io::Write as _;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => answered,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => answered,
        Err(e) => {
            eprintln!("arthron: writing stdout: {e}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

#[derive(Parser)]
#[command(name = "arthron", about = "Local-first code intelligence", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build (or refresh) the graph for a repository and print per-language
    /// resolution rates.
    ///
    /// Settings may also come from `arthron.toml` at the repository root; a
    /// flag given here wins over the file.
    #[command(after_long_help = json::CONFIG_HELP)]
    Scan {
        /// Repository root.
        path: PathBuf,
        /// Database file (default: the config's `db`, else
        /// <path>/.arthron/graph.redb).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Print one JSON document instead of the report.
        #[arg(long, long_help = json::HELP)]
        json: bool,
    },
    /// Scan a corpus and compare its counts against a committed baseline.
    ///
    /// Exit codes: 0 pass (or a successful --rebase), 1 gate failure, 2 usage
    /// or I/O error. The baseline's `corpus` and `commit` fields are
    /// provenance: printed, never verified — a vendored corpus snapshot
    /// carries no git metadata to check them against.
    ///
    /// `arthron.toml` at the corpus root is read for its globs and its
    /// `[tracks]` table, so a gate measures the file set a scan measures. Its
    /// `db` key is deliberately ignored: a gate is only meaningful against a
    /// cold store, and a config that pointed it at a warm one would move the
    /// number without moving the graph.
    #[command(after_long_help = json::CONFIG_HELP)]
    Gate {
        /// Corpus root.
        path: PathBuf,
        /// The language this gate measures. Rates are per language and never
        /// aggregated, so the baseline and the tally are both this one
        /// language's.
        #[arg(long, default_value = "go")]
        language: String,
        /// Baseline file to compare against, or to write with --rebase.
        #[arg(long)]
        baseline: PathBuf,
        /// Database file. Default: a fresh temporary store, deleted after the
        /// run. The gate is only meaningful against a cold store, so pass
        /// this only to keep the graph for inspection.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Overwrite the baseline with what this run measured instead of
        /// comparing against it. The ratchet moves up only by a deliberate
        /// commit; this is that commit's mechanism.
        #[arg(long)]
        rebase: bool,
        /// Commit to record as provenance when rebasing. Never verified.
        /// Defaults to the value already in the baseline file, or "unknown".
        #[arg(long)]
        commit: Option<String>,
        /// Print one JSON document instead of the report and the verdict.
        #[arg(long, long_help = json::HELP)]
        json: bool,
    },
    /// Ask the stored graph about a name.
    ///
    /// The store is opened read-only: a query never creates it, never
    /// rebuilds it, and fails at once rather than waiting when a scan is
    /// holding it for writing.
    ///
    /// A name may be a full FQN or any suffix of one that starts at a
    /// separator. A suffix several nodes end is answered with all of them —
    /// exit code 1, and the list to choose from — because picking one would
    /// be a guess.
    ///
    /// Exit codes: 0 answered, 1 no match or ambiguous, 2 usage or I/O error.
    Query {
        #[command(subcommand)]
        verb: QueryVerb,
        /// Database file (default: the config's `db`, else
        /// .arthron/graph.redb).
        #[arg(long, global = true)]
        db: Option<PathBuf>,
        /// Print one JSON document instead of the report.
        #[arg(long, global = true, long_help = json::HELP)]
        json: bool,
    },
    /// Serve the graph to an agent over the Model Context Protocol, on stdio.
    ///
    /// JSON-RPC 2.0, one message per line, stdin in and stdout out. Every tool
    /// returns the same document `--json` prints, from the same library calls
    /// the other commands make: there is no second answer for agents.
    ///
    /// No socket is opened and no address is bound.
    #[command(after_long_help = mcp::HELP)]
    Mcp {
        /// The graph the query tools read (default: the config's `db`, else
        /// .arthron/graph.redb). `scan_repo` writes wherever its own
        /// arguments say.
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

/// The three questions the graph answers about a name.
#[derive(Subcommand)]
enum QueryVerb {
    /// The definition record and every site that declares it.
    Def {
        /// A full FQN, or a suffix of one starting at a separator.
        name: String,
    },
    /// Every stored reference row that resolved to this name.
    Refs {
        /// A full FQN, or a suffix of one starting at a separator.
        name: String,
    },
    /// What transitively reaches this name, layer by layer.
    Impact {
        /// A full FQN, or a suffix of one starting at a separator.
        name: String,
        /// How many hops of the reverse closure to walk.
        #[arg(long, default_value_t = DEFAULT_IMPACT_DEPTH)]
        depth: u32,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { path, db, json } => run_scan(&path, db, json),
        Command::Gate {
            path,
            language,
            baseline,
            db,
            rebase,
            commit,
            json,
        } => run_gate(
            &path,
            &language,
            &baseline,
            db.as_deref(),
            rebase,
            commit.as_deref(),
            json,
        ),
        Command::Query { verb, db, json } => match working_db(db) {
            Ok(db_path) => run_query(&verb, &db_path, json),
            Err(e) => {
                eprintln!("arthron: {e}");
                ExitCode::from(EXIT_USAGE)
            }
        },
        Command::Mcp { db } => match working_db(db) {
            Ok(db_path) => run_mcp(db_path),
            Err(e) => {
                eprintln!("arthron: {e}");
                ExitCode::from(EXIT_USAGE)
            }
        },
    }
}

/// The graph a command with no path argument reads.
///
/// `query` and `mcp` both take a name rather than a repository, so their
/// configuration is the working directory's — the repository you are standing
/// in, which is the one whose graph `.arthron/graph.redb` names. The flag wins
/// over the file, as everywhere else.
///
/// Winning means the file is not read at all. `db` is the only key either
/// command takes from it, so a `--db` that names the store leaves nothing for
/// the file to say — and a syntax error in a config file the run has no
/// business reading must not be what stops an agent's MCP server from
/// starting. `scan` and `gate` still read it unconditionally: they take
/// `include`, `exclude` and `[tracks]` from it, and those decide what is
/// measured no matter where the store lives.
fn working_db(db: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = db {
        return Ok(path);
    }
    let root = PathBuf::from(".");
    let config = Config::load(&root)?;
    Ok(config
        .db_path(&root)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DB)))
}

/// Answer MCP messages on stdin until end of input.
///
/// The store is not opened here: a client's first call is usually `scan_repo`,
/// which creates it, so a server whose graph does not exist yet must still
/// start.
fn run_mcp(db_path: PathBuf) -> ExitCode {
    let server = mcp::Server::new(db_path);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match server.run(&mut stdin.lock(), &mut stdout.lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // A broken pipe or an unreadable stdin: the transport itself
            // failed, so there is nowhere to send a JSON-RPC error.
            eprintln!("arthron: {e}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Build or refresh a repository's graph and report what it now holds.
fn run_scan(path: &Path, db: Option<PathBuf>, as_json: bool) -> ExitCode {
    let config = match Config::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("arthron: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // The flag wins over the file: a person naming a store on the command
    // line has said something more specific than the repository has.
    let db_path = db
        .or_else(|| config.db_path(path))
        .unwrap_or_else(|| path.join(".arthron/graph.redb"));
    if let Some(parent) = db_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("arthron: creating {}: {e}", parent.display());
        return ExitCode::FAILURE;
    }
    match scan_repo_with(path, &db_path, &config) {
        Ok(report) => {
            warn_if_include_matched_nothing(&config, &report);
            if as_json {
                print_json(&json::scan(&report, &config), ExitCode::SUCCESS)
            } else {
                emit(&report_text(&report), ExitCode::SUCCESS)
            }
        }
        Err(e) => {
            eprintln!("arthron: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Print one JSON document and exit with `answered`, or say why it could not
/// be rendered.
///
/// A serialisation failure is [`EXIT_USAGE`] and not a silent empty line: the
/// run measured something, and a caller must never read "no output" as "no
/// findings". `answered` is the code the *answer* carries — a gate failure or
/// an ambiguous query is a document and a non-zero exit at the same time.
fn print_json(doc: &serde_json::Value, answered: ExitCode) -> ExitCode {
    match json::render(doc) {
        Ok(text) => emit(&format!("{text}\n"), answered),
        Err(e) => {
            eprintln!("arthron: {e}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Say so when a whitelist was set and the scan then measured nothing.
///
/// A scan whose `include` globs match no file reads zero files and reports it
/// as a clean run: rate `n/a`, exit 0, `"languages": {}`. That is the same
/// document a repository with no source in it produces, and the two need
/// telling apart — one of them means "your globs are wrong". `gate` already
/// refuses a zero denominator, so this is the `scan` half of the same guard.
///
/// A warning on stderr and never a failure: measuring nothing is a legitimate
/// answer about an empty tree, and this cannot tell the two apart with
/// certainty — only that the configuration is the likely cause. The exit code
/// and the document are untouched.
fn warn_if_include_matched_nothing(config: &Config, report: &arthron::store::Report) {
    if config.include.is_empty() {
        return;
    }
    let measured: u64 = report
        .per_lang
        .values()
        .map(|t| t.resolved + t.external + t.local_binding + t.unresolved_total())
        .sum();
    if measured > 0 {
        return;
    }
    eprintln!(
        "arthron: `include` is set and this scan measured no reference at all — \
         the globs may match no file. A bare directory name matches the \
         directory and not the files under it: `include = [\"src\"]` reads \
         nothing, `include = [\"src/**\"]` reads the subtree.",
    );
}

/// A scratch directory removed when the run ends.
///
/// The gate must measure a cold store — a warm one would gate on whatever the
/// last run happened to leave behind — so the default store is created fresh
/// and thrown away.
struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch_dir() -> Result<ScratchDir, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("reading the clock: {e}"))?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("arthron-gate-{}-{nanos}", std::process::id()));
    // `create_dir`, not `create_dir_all`: a name that already exists would be
    // a warm store, and the gate must not silently measure one.
    std::fs::create_dir(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(ScratchDir(dir))
}

fn run_gate(
    path: &Path,
    language: &str,
    baseline_path: &Path,
    db: Option<&Path>,
    rebase: bool,
    commit: Option<&str>,
    as_json: bool,
) -> ExitCode {
    // Only a language whose track is live can be gated, and both checks
    // answer before anything is read. A registered-but-disabled track
    // contributes no row, so its tally is zeros: letting the name through
    // would spend a whole cold scan to arrive at a usage error that was
    // already knowable, and `Lang::ALL` names far more languages than this
    // build can measure.
    let gateable: Vec<&str> = REGISTRY
        .iter()
        .filter(|t| t.is_enabled())
        .flat_map(|t| t.langs)
        .map(|l| l.name())
        .collect();
    let Some(lang) = Lang::ALL.iter().copied().find(|l| l.name() == language) else {
        eprintln!(
            "arthron: unknown language `{language}`; one of: {}",
            gateable.join(", ")
        );
        return ExitCode::from(EXIT_USAGE);
    };
    if !gateable.contains(&lang.name()) {
        eprintln!(
            "arthron: language `{language}` is registered but its track is not live in \
             this build, so there is nothing to gate; one of: {}",
            gateable.join(", "),
        );
        return ExitCode::from(EXIT_USAGE);
    }
    // The corpus's own config decides the file set and which tracks run, so
    // the gate measures what a scan of the same tree measures. Its `db` key is
    // not read: see the command's help.
    let config = match Config::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("arthron: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // Read the baseline before scanning. A malformed file is a usage error,
    // and finding that out after a multi-minute scan helps nobody.
    let existing = match std::fs::read_to_string(baseline_path) {
        Ok(text) => match parse_baseline(&text) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("arthron: {}: {e}", baseline_path.display());
                return ExitCode::from(EXIT_USAGE);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("arthron: reading {}: {e}", baseline_path.display());
            return ExitCode::from(EXIT_USAGE);
        }
    };

    if let Some(b) = &existing
        && b.language != lang.name()
    {
        // Rates are per language and never aggregated, so a baseline for one
        // language must never be compared against another's scan.
        eprintln!(
            "arthron: {}: baseline is for language `{}`, this scan measures `{}`",
            baseline_path.display(),
            b.language,
            lang.name(),
        );
        return ExitCode::from(EXIT_USAGE);
    }

    // `_scratch` is bound, not discarded: its `Drop` is what removes the
    // temporary store, and it must outlive the scan.
    let (db_path, _scratch) = match db {
        Some(p) => (p.to_path_buf(), None),
        None => match scratch_dir() {
            Ok(d) => (d.0.join("gate.redb"), Some(d)),
            Err(e) => {
                eprintln!("arthron: {e}");
                return ExitCode::from(EXIT_USAGE);
            }
        },
    };
    if let Some(parent) = db_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("arthron: creating {}: {e}", parent.display());
        return ExitCode::from(EXIT_USAGE);
    }

    // Every enabled track, then one language's tally out of the result: the
    // gate is per language and a baseline names the one it measures, so
    // scanning the whole registry never turns into gating a combined number.
    let report = match scan_repo_with(path, &db_path, &config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("arthron: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // The report is the head of the text answer, and the verdict its tail:
    // one string, one write, so a reader that leaves after the first line
    // cannot turn the second into a panic.
    let mut text = if as_json {
        String::new()
    } else {
        report_text(&report)
    };

    let tally = report
        .per_lang
        .get(&lang.code())
        .cloned()
        .unwrap_or_default();
    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };

    let shown = baseline_path.display().to_string();
    if rebase {
        let written = match write_baseline(
            baseline_path,
            path,
            lang,
            &measured,
            existing.as_ref(),
            commit,
        ) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("arthron: {e}");
                return ExitCode::from(EXIT_USAGE);
            }
        };
        if as_json {
            let doc = json::gate(&json::GateOutput {
                language: lang.name(),
                baseline_path: &shown,
                corpus: &written.corpus,
                commit: &written.commit,
                config: &config,
                report: &report,
                // The baseline side of a re-base is what was just written,
                // which is the measured side. Reporting the counts it
                // replaced would read as a comparison that never happened.
                baseline: written.counts,
                measured,
                verdict: None,
            });
            print_json(&doc, ExitCode::SUCCESS)
        } else {
            outln!(
                text,
                "gate: wrote {shown} at {} ({})",
                written.commit,
                written.corpus,
            );
            emit(&text, ExitCode::SUCCESS)
        }
    } else {
        let Some(baseline) = existing else {
            eprintln!("arthron: {shown} does not exist; record it with --rebase",);
            return ExitCode::from(EXIT_USAGE);
        };
        let verdict = evaluate(&baseline, &measured);
        if as_json {
            let doc = json::gate(&json::GateOutput {
                language: lang.name(),
                baseline_path: &shown,
                corpus: &baseline.corpus,
                commit: &baseline.commit,
                config: &config,
                report: &report,
                baseline: baseline.counts,
                measured,
                verdict: Some(&verdict),
            });
            print_json(&doc, verdict_exit(&verdict))
        } else {
            report_verdict(text, &baseline, &verdict, &measured, baseline_path)
        }
    }
}

/// The exit code a verdict carries.
///
/// One function so the two output modes cannot disagree about whether a run
/// passed — the JSON document and the exit code are read by the same script.
fn verdict_exit(verdict: &GateVerdict) -> ExitCode {
    match verdict {
        GateVerdict::Pass { .. } => ExitCode::SUCCESS,
        GateVerdict::Fail(_) => ExitCode::from(EXIT_GATE_FAILED),
        GateVerdict::Error(_) => ExitCode::from(EXIT_USAGE),
    }
}

/// Write the baseline this run measured, and hand back what was written.
///
/// Returns the record rather than an exit code so both output modes report the
/// same file: the text line and the JSON document are rendered from this one
/// value instead of each re-deriving it.
fn write_baseline(
    baseline_path: &Path,
    corpus: &Path,
    lang: Lang,
    measured: &Counts,
    existing: Option<&Baseline>,
    commit: Option<&str>,
) -> Result<Baseline, String> {
    // A baseline of all zeros looks exactly as authoritative as a correct
    // one, and every later gate run would bless it. Refuse.
    if measured.total() == 0 {
        return Err(format!(
            "refusing to write {}: this scan counted no references at all",
            baseline_path.display(),
        ));
    }
    let corpus = corpus.display().to_string();
    let commit = commit
        .map(str::to_string)
        .or_else(|| existing.map(|b| b.commit.clone()))
        .unwrap_or_else(|| "unknown".to_string());
    for (field, value) in [("corpus", &corpus), ("commit", &commit)] {
        if !is_renderable(value) {
            return Err(format!(
                "`{field}` contains a quote or a newline, which this baseline \
                 format cannot represent: {value:?}",
            ));
        }
    }
    let baseline = Baseline {
        format: FORMAT,
        corpus,
        commit,
        language: lang.name().to_string(),
        counts: *measured,
    };
    if let Some(parent) = baseline_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(baseline_path, render_baseline(&baseline))
        .map_err(|e| format!("writing {}: {e}", baseline_path.display()))?;
    Ok(baseline)
}

fn report_verdict(
    mut text: String,
    baseline: &Baseline,
    verdict: &GateVerdict,
    measured: &Counts,
    baseline_path: &Path,
) -> ExitCode {
    outln!(
        text,
        "gate: {} ({} at {})",
        baseline_path.display(),
        baseline.corpus,
        baseline.commit,
    );
    match verdict {
        GateVerdict::Pass { improved } => {
            if *improved {
                outln!(
                    text,
                    "gate: pass — {} improved to {}; lock it in with --rebase",
                    show_rate(&baseline.counts),
                    show_rate(measured),
                );
            } else {
                outln!(text, "gate: pass — {} held", show_rate(measured));
            }
            emit(&text, ExitCode::SUCCESS)
        }
        GateVerdict::Fail(failures) => {
            let code = emit(&text, ExitCode::from(EXIT_GATE_FAILED));
            for failure in failures {
                eprintln!("gate: FAIL — {failure}");
            }
            code
        }
        GateVerdict::Error(e) => {
            let code = emit(&text, ExitCode::from(EXIT_USAGE));
            eprintln!("gate: error — {e}");
            code
        }
    }
}

/// The rate as a display string. Every comparison the gate makes is exact
/// integer arithmetic; this is for humans only.
fn show_rate(c: &Counts) -> String {
    match resolution_rate(c.resolved, c.unresolved) {
        Some(r) => format!("{:.1}%", r * 100.0),
        None => "n/a (nothing to measure)".to_string(),
    }
}

fn report_text(report: &arthron::store::Report) -> String {
    let mut text = String::new();
    // A scanned language with no reference rows still gets a line. The report
    // is keyed off references, so printing only what it contains would make
    // "nothing to measure" indistinguishable from a clean run, and those are
    // different facts.
    let empty = LangTally::default();
    let mut lang_codes: BTreeSet<u8> = report.per_lang.keys().copied().collect();
    lang_codes.insert(Lang::Go.code());
    for lang_code in lang_codes {
        let tally = report.per_lang.get(&lang_code).unwrap_or(&empty);
        let lang = Lang::from_code(lang_code).map_or("unknown", Lang::name);
        let unresolved = tally.unresolved_total();
        let rate = match resolution_rate(tally.resolved, unresolved) {
            Some(r) => format!("{:.1}%", r * 100.0),
            None => "n/a (nothing to measure)".to_string(),
        };
        // `local-binding` gets its own column rather than a share of
        // `unresolved`: it is policy-caused — locals are not nodes by
        // design — so it is neither a linking success nor a language-support
        // failure, and it sits outside both terms of the rate.
        outln!(
            text,
            "{lang:<12} resolved {:<8} external {:<8} local-binding {:<8} \
             unresolved {:<8} rate {rate}",
            tally.resolved,
            tally.external,
            tally.local_binding,
            unresolved
        );
        for (code, count) in &tally.unresolved {
            outln!(text, "             {} {count}", reason_name(*code));
        }
    }
    // Data, not a verdict: two files declaring one FQN is a fact about the
    // repository, printed so it can be looked at, never gating anything.
    if report.fqn_collisions > 0 {
        outln!(text, "fqn collisions {}", report.fqn_collisions);
    }
    // Also data, and the one line that says the counts above were taken over
    // fewer files than the walk found. The paths are what makes it
    // actionable, so a handful are printed; the rest are in `--json`, whole.
    if !report.file_errors.is_empty() {
        outln!(text, "file errors {}", report.file_errors.len());
        for e in report.file_errors.iter().take(SHOWN_FILE_ERRORS) {
            outln!(text, "             {}: {}", e.path, e.message);
        }
        if let Some(rest) = report.file_errors.len().checked_sub(SHOWN_FILE_ERRORS)
            && rest > 0
        {
            outln!(text, "             … and {rest} more (see --json)");
        }
    }
    text
}

/// How many unreadable files the text report names before it stops listing.
///
/// The count is always exact; this bounds the *listing*, so a tree with a
/// permissions problem across thousands of files does not bury the rates
/// under its own paths. `--json` carries every one.
const SHOWN_FILE_ERRORS: usize = 10;

/// Width of the label column, matching [`print_report`]'s.
const LABEL: usize = 12;

fn run_query(verb: &QueryVerb, db_path: &Path, as_json: bool) -> ExitCode {
    let store = match ReadStore::open(db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("arthron: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let index = match NameIndex::build(&store) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("arthron: {}: {e}", db_path.display());
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let (verb_name, name) = match verb {
        QueryVerb::Def { name } => ("def", name),
        QueryVerb::Refs { name } => ("refs", name),
        QueryVerb::Impact { name, .. } => ("impact", name),
    };
    let (node, shadowed) = match select(&index, verb_name, name, as_json) {
        Ok(selected) => selected,
        Err(code) => return code,
    };

    let outcome = match verb {
        QueryVerb::Def { .. } => show_definition(&store, &node, &shadowed, name, as_json),
        QueryVerb::Refs { .. } => show_references(&store, &node, &shadowed, name, as_json),
        QueryVerb::Impact { depth, .. } => {
            show_impact(&store, &node, &shadowed, name, *depth, as_json)
        }
    };
    match outcome {
        Ok(code) => code,
        Err(e) => {
            eprintln!("arthron: {e}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Narrow a name to the one node it selects, or print why it does not.
///
/// Hands back the node *and* the candidates an exact match won over, because
/// every verb has to report the second alongside its answer: winning outright
/// is a choice between readings of the name, and a choice nobody is told about
/// is the guess this project forbids.
///
/// Both failures print to stdout and exit 1: an ambiguous name *is* an
/// answer — here are the nodes, pick one — and burying it on stderr would
/// hide the list a person needs in order to re-run. Under `--json` they are
/// documents for the same reason.
fn select(
    index: &NameIndex,
    verb: &str,
    name: &str,
    as_json: bool,
) -> Result<(Match, Vec<Match>), ExitCode> {
    let mut found = index.lookup(name);
    if found.matches.len() == 1 {
        return Ok((found.matches.remove(0), found.shadowed));
    }
    let hits = found.matches;
    let no_answer = ExitCode::from(EXIT_NO_ANSWER);
    if as_json {
        let doc = if hits.is_empty() {
            json::query_no_match(verb, name)
        } else {
            // Several exact matches and hidden suffix candidates at once: the
            // ambiguous list is already the whole answer, so the shadowed
            // ones join it rather than being reported separately.
            let mut all = hits;
            all.extend(found.shadowed);
            json::query_ambiguous(verb, name, &all)
        };
        return Err(print_json(&doc, no_answer));
    }
    let mut text = String::new();
    if hits.is_empty() {
        // An empty graph and a name that is not in a populated one are
        // different facts, and only one of them means "fix the name".
        if index.is_empty() {
            outln!(
                text,
                "no match for {name:?} — the store holds no nodes; run `arthron scan`"
            );
        } else {
            outln!(text, "no match for {name:?}");
        }
    } else {
        let mut all = hits;
        all.extend(found.shadowed);
        outln!(text, "ambiguous: {} matches for {name:?}", all.len());
        let width = all.iter().map(|m| m.name.len()).max().unwrap_or(0);
        for m in &all {
            outln!(text, "  {:<width$}  {}", m.name, kind_name(m.kind));
        }
    }
    Err(emit(&text, no_answer))
}

/// The note every verb prints beside its answer when the name also ended
/// other nodes.
///
/// Empty for almost every query. When it is not, the answer is one reading of
/// the name and this is the list of the others — spell more of the name to
/// reach them.
fn shadow_note(query: &str, shadowed: &[Match]) -> String {
    let mut text = String::new();
    if shadowed.is_empty() {
        return text;
    }
    outln!(
        text,
        "{:<LABEL$} {query:?} is an exact name, so it selected the node above; \
         it also ends {} other node(s):",
        "also",
        shadowed.len(),
    );
    let width = shadowed.iter().map(|m| m.name.len()).max().unwrap_or(0);
    for m in shadowed {
        outln!(text, "  {:<width$}  {}", m.name, kind_name(m.kind));
    }
    text
}

/// A node kind as the report style spells it.
fn kind_name(kind: NodeKind) -> String {
    match kind {
        NodeKind::Definition(k) => k.name().to_string(),
        NodeKind::Package => "package".to_string(),
        NodeKind::External => "external".to_string(),
        NodeKind::Missing => "missing (an edge names it; no node declares it)".to_string(),
    }
}

fn show_definition(
    store: &ReadStore,
    node: &Match,
    shadowed: &[Match],
    query: &str,
    as_json: bool,
) -> Result<ExitCode, String> {
    let Some(def) = definition(store, &node.id)? else {
        // The index came from the node table, so this is unreachable short of
        // the store changing underneath — which a read-only open forbids.
        return Err(format!("{}: the node vanished between reads", node.name));
    };
    if as_json {
        return Ok(print_json(
            &json::query_definition(query, &def, shadowed),
            ExitCode::SUCCESS,
        ));
    }
    let mut text = String::new();
    outln!(text, "{:<LABEL$} {}", "definition", def.node.name);
    outln!(text, "{:<LABEL$} {}", "kind", kind_name(def.node.kind));
    if def.declarations.is_empty() {
        // Only reachable for a node whose sites were all forgotten, which the
        // store deletes outright — printed rather than assumed away.
        outln!(text, "{:<LABEL$} none recorded", "declared");
    }
    // One line per site, not a count: a node two files declare is a fact
    // about the repository and collapsing it would hide the twin.
    for site in &def.declarations {
        outln!(text, "{:<LABEL$} {}:{}", "declared", site.file, site.line);
    }
    for target in &def.targets {
        outln!(
            text,
            "{:<LABEL$} {}  {}",
            "alias of",
            target.name,
            kind_name(target.kind)
        );
    }
    text.push_str(&shadow_note(query, shadowed));
    Ok(emit(&text, ExitCode::SUCCESS))
}

fn show_references(
    store: &ReadStore,
    node: &Match,
    shadowed: &[Match],
    query: &str,
    as_json: bool,
) -> Result<ExitCode, String> {
    let sites = references(store, &node.id)?;
    if as_json {
        return Ok(print_json(
            &json::query_references(query, node, &sites, shadowed),
            ExitCode::SUCCESS,
        ));
    }
    let mut text = String::new();
    if sites.is_empty() {
        outln!(
            text,
            "{:<LABEL$} {} — no stored row resolves here",
            "references",
            node.name
        );
        text.push_str(&shadow_note(query, shadowed));
        return Ok(emit(&text, ExitCode::SUCCESS));
    }
    let occurrences: u64 = sites.iter().map(|s| u64::from(s.count)).sum();
    outln!(
        text,
        "{:<LABEL$} {} — {} row(s), {occurrences} occurrence(s)",
        "references",
        node.name,
        sites.len(),
    );
    let places: Vec<String> = sites
        .iter()
        .map(|s| format!("{}:{}", s.file, s.line))
        .collect();
    let place_w = width(places.iter().map(String::as_str));
    let kind_w = width(sites.iter().map(ref_kind_name));
    let encloser_w = width(sites.iter().map(|s| s.enclosing.as_str()));
    let target_w = width(sites.iter().map(|s| s.raw_target.as_str()));
    for (site, place) in sites.iter().zip(&places) {
        outln!(
            text,
            "  {place:<place_w$}  {:<kind_w$}  {:<encloser_w$}  {:<target_w$}  x{:<4} {}",
            ref_kind_name(site),
            site.enclosing,
            site.raw_target,
            site.count,
            outcome_name(&site.outcome),
        );
    }
    text.push_str(&shadow_note(query, shadowed));
    Ok(emit(&text, ExitCode::SUCCESS))
}

fn show_impact(
    store: &ReadStore,
    node: &Match,
    shadowed: &[Match],
    query: &str,
    depth: u32,
    as_json: bool,
) -> Result<ExitCode, String> {
    let found = impact(store, &node.id, depth)?;
    if as_json {
        return Ok(print_json(
            &json::query_impact(query, node, depth, &found, shadowed),
            ExitCode::SUCCESS,
        ));
    }
    let Impact { layers, truncated } = found;
    let total: usize = layers.iter().map(Vec::len).sum();
    let mut text = String::new();
    outln!(
        text,
        "{:<LABEL$} {} — depth {depth}, {total} node(s)",
        "impact",
        node.name,
    );
    if layers.is_empty() && !truncated {
        outln!(text, "  nothing in the graph reaches it");
    }
    for (hop, layer) in layers.iter().enumerate() {
        outln!(text, "depth {:<6} {} node(s)", hop + 1, layer.len());
        let name_w = width(layer.iter().map(|m| m.name.as_str()));
        for m in layer {
            outln!(text, "  {:<name_w$}  {}", m.name, kind_name(m.kind));
        }
    }
    // A bounded closure and an exhausted one print the same layers, so the
    // difference has to be said out loud rather than inferred from the count.
    if truncated {
        outln!(
            text,
            "{:<LABEL$} the walk stopped at depth {depth}; more reaches it beyond",
            "truncated"
        );
    }
    text.push_str(&shadow_note(query, shadowed));
    Ok(emit(&text, ExitCode::SUCCESS))
}

/// The widest of a set of column values, for aligned output.
fn width<'a>(values: impl Iterator<Item = &'a str>) -> usize {
    values.map(str::len).max().unwrap_or(0)
}

/// A reference kind as the report style spells it. A stored code no variant
/// carries is shown as the code, never guessed at.
fn ref_kind_name(site: &RefSite) -> &'static str {
    site.kind.map_or("unknown", |k| k.name())
}

/// The stored outcome as one column.
fn outcome_name(outcome: &StoredOutcome) -> String {
    match outcome {
        StoredOutcome::Resolved(_) => "resolved".to_string(),
        StoredOutcome::External(pkg) => format!("external {pkg}"),
        StoredOutcome::Unresolved(reason) => format!("unresolved {}", reason_name(*reason)),
    }
}
