//! `arthron` CLI. Printing only — analysis logic lives in the library.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use arthron::model::{Lang, reason_name};
use arthron::pipeline::scan_go;
use arthron::resolution_rate;
use arthron::store::LangTally;

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
            match scan_go(&path, &db_path) {
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
        println!(
            "{lang:<12} resolved {:<8} external {:<8} unresolved {:<8} rate {rate}",
            tally.resolved, tally.external, unresolved
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
