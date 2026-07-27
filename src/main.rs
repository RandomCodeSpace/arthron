//! `arthron` CLI. Printing only — analysis logic lives in the library.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use arthron::gate::{
    Baseline, Counts, FORMAT, GateVerdict, evaluate, is_renderable, parse_baseline, render_baseline,
};
use arthron::model::{Lang, reason_name};
use arthron::pipeline::scan_repo;
use arthron::resolution_rate;
use arthron::store::LangTally;

/// Exit code for a gate regression: the run worked, the numbers are worse.
const EXIT_GATE_FAILED: u8 = 1;
/// Exit code for usage and I/O problems: nothing was measured, so neither a
/// pass nor a failure may be reported.
const EXIT_USAGE: u8 = 2;

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
    Scan {
        /// Repository root (must contain go.mod).
        path: PathBuf,
        /// Database file (default: <path>/.arthron/graph.redb).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Scan a corpus and compare its counts against a committed baseline.
    ///
    /// Exit codes: 0 pass (or a successful --rebase), 1 gate failure, 2 usage
    /// or I/O error. The baseline's `corpus` and `commit` fields are
    /// provenance: printed, never verified — a vendored corpus snapshot
    /// carries no git metadata to check them against.
    Gate {
        /// Corpus root (must contain go.mod).
        path: PathBuf,
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
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { path, db } => {
            let db_path = db.unwrap_or_else(|| path.join(".arthron/graph.redb"));
            if let Some(parent) = db_path.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                eprintln!("arthron: creating {}: {e}", parent.display());
                return ExitCode::FAILURE;
            }
            match scan_repo(&path, &db_path) {
                Ok(report) => {
                    print_report(&report);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("arthron: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Gate {
            path,
            baseline,
            db,
            rebase,
            commit,
        } => run_gate(&path, &baseline, db.as_deref(), rebase, commit.as_deref()),
    }
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
    baseline_path: &Path,
    db: Option<&Path>,
    rebase: bool,
    commit: Option<&str>,
) -> ExitCode {
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
        && b.language != Lang::Go.name()
    {
        // Rates are per language and never aggregated, so a baseline for one
        // language must never be compared against another's scan.
        eprintln!(
            "arthron: {}: baseline is for language `{}`, this scan measures `{}`",
            baseline_path.display(),
            b.language,
            Lang::Go.name(),
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
    let report = match scan_repo(path, &db_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("arthron: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    print_report(&report);

    let tally = report
        .per_lang
        .get(&Lang::Go.code())
        .cloned()
        .unwrap_or_default();
    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };

    if rebase {
        write_baseline(baseline_path, path, &measured, existing.as_ref(), commit)
    } else {
        let Some(baseline) = existing else {
            eprintln!(
                "arthron: {} does not exist; record it with --rebase",
                baseline_path.display(),
            );
            return ExitCode::from(EXIT_USAGE);
        };
        report_verdict(&baseline, &measured, baseline_path)
    }
}

fn write_baseline(
    baseline_path: &Path,
    corpus: &Path,
    measured: &Counts,
    existing: Option<&Baseline>,
    commit: Option<&str>,
) -> ExitCode {
    // A baseline of all zeros looks exactly as authoritative as a correct
    // one, and every later gate run would bless it. Refuse.
    if measured.total() == 0 {
        eprintln!(
            "arthron: refusing to write {}: this scan counted no references at all",
            baseline_path.display(),
        );
        return ExitCode::from(EXIT_USAGE);
    }
    let corpus = corpus.display().to_string();
    let commit = commit
        .map(str::to_string)
        .or_else(|| existing.map(|b| b.commit.clone()))
        .unwrap_or_else(|| "unknown".to_string());
    for (field, value) in [("corpus", &corpus), ("commit", &commit)] {
        if !is_renderable(value) {
            eprintln!(
                "arthron: `{field}` contains a quote or a newline, which this baseline \
                 format cannot represent: {value:?}",
            );
            return ExitCode::from(EXIT_USAGE);
        }
    }
    let baseline = Baseline {
        format: FORMAT,
        corpus,
        commit,
        language: Lang::Go.name().to_string(),
        counts: *measured,
    };
    if let Some(parent) = baseline_path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("arthron: creating {}: {e}", parent.display());
        return ExitCode::from(EXIT_USAGE);
    }
    match std::fs::write(baseline_path, render_baseline(&baseline)) {
        Ok(()) => {
            println!(
                "gate: wrote {} at {} ({})",
                baseline_path.display(),
                baseline.commit,
                baseline.corpus,
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("arthron: writing {}: {e}", baseline_path.display());
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn report_verdict(baseline: &Baseline, measured: &Counts, baseline_path: &Path) -> ExitCode {
    println!(
        "gate: {} ({} at {})",
        baseline_path.display(),
        baseline.corpus,
        baseline.commit,
    );
    match evaluate(baseline, measured) {
        GateVerdict::Pass { improved } => {
            if improved {
                println!(
                    "gate: pass — {} improved to {}; lock it in with --rebase",
                    show_rate(&baseline.counts),
                    show_rate(measured),
                );
            } else {
                println!("gate: pass — {} held", show_rate(measured));
            }
            ExitCode::SUCCESS
        }
        GateVerdict::Fail(failures) => {
            for failure in &failures {
                eprintln!("gate: FAIL — {failure}");
            }
            ExitCode::from(EXIT_GATE_FAILED)
        }
        GateVerdict::Error(e) => {
            eprintln!("gate: error — {e}");
            ExitCode::from(EXIT_USAGE)
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

fn print_report(report: &arthron::store::Report) {
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
        println!(
            "{lang:<12} resolved {:<8} external {:<8} local-binding {:<8} \
             unresolved {:<8} rate {rate}",
            tally.resolved, tally.external, tally.local_binding, unresolved
        );
        for (code, count) in &tally.unresolved {
            println!("             {} {count}", reason_name(*code));
        }
    }
    // Data, not a verdict: two files declaring one FQN is a fact about the
    // repository, printed so it can be looked at, never gating anything.
    if report.fqn_collisions > 0 {
        println!("fqn collisions {}", report.fqn_collisions);
    }
}
