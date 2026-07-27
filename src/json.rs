//! Machine-readable output: one JSON document per command invocation.
//!
//! # The shape is a contract
//!
//! These field names are public API from 0.0.1 and are versioned by
//! [`SCHEMA`]. A script that reads `resolved` must keep reading `resolved`, so
//! the names are chosen once here, documented in `--help`, and pinned by
//! `tests/json_shape.rs`, which compares whole documents rather than probing
//! for the keys it happens to care about — an accidental rename has to fail
//! the build rather than silently empty somebody's dashboard.
//!
//! # Conventions
//!
//! - **Field names are `lowercase_snake`.** Values are not: a reference kind
//!   is `type-use` and an unresolved reason is `NeedsTypeInference`, because
//!   those are the names the human report and the source already use, and one
//!   thing with two spellings is how a document and a log stop agreeing.
//! - **A stored code with no variant is `null`, never a guess.** The same rule
//!   the text report follows.
//! - **A language with no rows has no entry.** A rate of zero and the absence
//!   of any reference are different facts; the text report prints a Go line
//!   unconditionally as a reminder to a reader, and machine output has no
//!   reader to remind.
//! - **Only measurements are JSON.** A usage or I/O error is a line on stderr
//!   and a non-zero exit in both modes, because nothing was measured. An
//!   ambiguous or absent query name *is* a measurement and is a document.

use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::gate::{Counts, GateFailure, GateVerdict};
use crate::model::{Lang, RefKind, reason_name};
use crate::query::{Definition, Impact, Match, NodeKind, RefSite};
use crate::resolution_rate;
use crate::store::{FileError, Report, StoredOutcome};

/// The version of the JSON contract this build emits.
///
/// Present on every document as `schema`. It moves when a field is removed or
/// its meaning changes; adding a field does not move it, so a reader that
/// ignores unknown keys keeps working.
pub const SCHEMA: u32 = 1;

/// The `scan` document: what the store now holds, per language, and under
/// which file set it was measured.
pub fn scan(report: &Report, config: &Config) -> Value {
    json!({
        "schema": SCHEMA,
        "command": "scan",
        "config": settings(config),
        "languages": languages(report),
        "fqn_collisions": report.fqn_collisions,
        "file_errors": file_errors(&report.file_errors),
    })
}

/// The files a scan reached and could not read.
///
/// An array and not a count, because a count nobody can act on is a count
/// nobody looks at: the whole point of recording these is that the paths are
/// there. Empty on every clean scan, and always present — a reader never has
/// to tell "no failures" from "this build did not report them".
///
/// One entry per file, so the count is the array's length. Additive, so
/// [`SCHEMA`] does not move: a reader that ignores unknown keys is unaffected.
fn file_errors(errors: &[FileError]) -> Value {
    Value::Array(
        errors
            .iter()
            .map(|e| json!({ "path": e.path, "error": e.message }))
            .collect(),
    )
}

/// Everything one `gate` run decided, as the document needs it.
pub struct GateOutput<'a> {
    /// The one language this gate measured. Rates are never aggregated, so a
    /// gate document names exactly one.
    pub language: &'a str,
    /// The baseline file compared against, or written by a re-base.
    pub baseline_path: &'a str,
    /// The corpus the baseline records. Provenance, never verified.
    pub corpus: &'a str,
    /// The commit the baseline records. Provenance, never verified.
    pub commit: &'a str,
    /// The scanned root's own settings, as the run read them. Provenance for
    /// the *file set* the measured side was taken over.
    pub config: &'a Config,
    /// The whole store's per-language tallies from this run's scan.
    pub report: &'a Report,
    /// The counts being compared against — for a re-base, the counts just
    /// written.
    pub baseline: Counts,
    /// The counts this run measured.
    pub measured: Counts,
    /// The comparison, or `None` when the run re-based instead of comparing.
    pub verdict: Option<&'a GateVerdict>,
}

/// The `gate` document: the verdict, both sides of the comparison, and the
/// scan that produced the measured side.
pub fn gate(out: &GateOutput<'_>) -> Value {
    let (verdict, improved, failures, error) = match out.verdict {
        None => ("rebased", false, Vec::new(), Value::Null),
        Some(GateVerdict::Pass { improved }) => ("pass", *improved, Vec::new(), Value::Null),
        Some(GateVerdict::Fail(list)) => (
            "fail",
            false,
            list.iter().map(failure).collect(),
            Value::Null,
        ),
        Some(GateVerdict::Error(e)) => ("error", false, Vec::new(), json!(e)),
    };
    json!({
        "schema": SCHEMA,
        "command": "gate",
        "action": if out.verdict.is_some() { "compare" } else { "rebase" },
        "language": out.language,
        "baseline_path": out.baseline_path,
        "corpus": out.corpus,
        "commit": out.commit,
        "config": settings(out.config),
        "verdict": verdict,
        "improved": improved,
        "failures": failures,
        "error": error,
        "baseline": counts(out.baseline),
        "measured": counts(out.measured),
        "languages": languages(out.report),
        "fqn_collisions": out.report.fqn_collisions,
        // The same array the scan document carries, and for a stronger
        // reason: a gate's whole job is to say whether a corpus still
        // measures what it did, and a re-base writes a baseline from this
        // run. A corpus the scan could not fully read is a fact about the
        // measurement, so the document CI reads has to carry it too.
        "file_errors": file_errors(&out.report.file_errors),
    })
}

/// The `query def` document.
pub fn query_definition(query: &str, def: &Definition, shadowed: &[Match]) -> Value {
    let mut doc = envelope("def", query, "ok");
    doc.insert("shadowed".to_string(), nodes(shadowed));
    doc.insert("fqn".to_string(), json!(def.node.name));
    doc.insert("kind".to_string(), json!(kind_name(def.node.kind)));
    doc.insert(
        "declarations".to_string(),
        Value::Array(
            def.declarations
                .iter()
                .map(|site| json!({ "file": site.file, "line": site.line }))
                .collect(),
        ),
    );
    doc.insert(
        "aliases".to_string(),
        Value::Array(def.targets.iter().map(node).collect()),
    );
    Value::Object(doc)
}

/// The `query refs` document.
pub fn query_references(
    query: &str,
    selected: &Match,
    sites: &[RefSite],
    shadowed: &[Match],
) -> Value {
    let occurrences: u64 = sites.iter().map(|s| u64::from(s.count)).sum();
    let mut doc = envelope("refs", query, "ok");
    doc.insert("shadowed".to_string(), nodes(shadowed));
    doc.insert("fqn".to_string(), json!(selected.name));
    doc.insert("kind".to_string(), json!(kind_name(selected.kind)));
    doc.insert("rows".to_string(), json!(sites.len()));
    doc.insert("occurrences".to_string(), json!(occurrences));
    doc.insert(
        "references".to_string(),
        Value::Array(sites.iter().map(site).collect()),
    );
    Value::Object(doc)
}

/// The `query impact` document.
pub fn query_impact(
    query: &str,
    selected: &Match,
    depth: u32,
    found: &Impact,
    shadowed: &[Match],
) -> Value {
    let total: usize = found.layers.iter().map(Vec::len).sum();
    let mut doc = envelope("impact", query, "ok");
    doc.insert("shadowed".to_string(), nodes(shadowed));
    doc.insert("fqn".to_string(), json!(selected.name));
    doc.insert("kind".to_string(), json!(kind_name(selected.kind)));
    doc.insert("depth".to_string(), json!(depth));
    doc.insert("total".to_string(), json!(total));
    doc.insert("truncated".to_string(), json!(found.truncated));
    doc.insert(
        "layers".to_string(),
        Value::Array(
            found
                .layers
                .iter()
                .enumerate()
                .map(|(hop, layer)| {
                    json!({
                        "depth": hop + 1,
                        "nodes": layer.iter().map(node).collect::<Vec<Value>>(),
                    })
                })
                .collect(),
        ),
    );
    Value::Object(doc)
}

/// The document for a name the graph holds no node under.
///
/// `matches` is present and empty rather than absent: a reader branching on
/// `status` finds the same key in both non-`ok` cases.
pub fn query_no_match(verb: &str, query: &str) -> Value {
    let mut doc = envelope(verb, query, "no_match");
    doc.insert("matches".to_string(), Value::Array(Vec::new()));
    Value::Object(doc)
}

/// The document for a name that selects more than one node.
///
/// Every candidate is listed, because picking one would be the guess the
/// resolver itself is forbidden from making.
pub fn query_ambiguous(verb: &str, query: &str, matches: &[Match]) -> Value {
    let mut doc = envelope(verb, query, "ambiguous");
    doc.insert(
        "matches".to_string(),
        Value::Array(matches.iter().map(node).collect()),
    );
    Value::Object(doc)
}

/// Render a document for printing.
///
/// Pretty rather than one line: these are read by people at least as often as
/// by scripts, and `jq` does not care either way. Key order is
/// `serde_json`'s — sorted — so two runs over one store print byte-identical
/// documents.
pub fn render(doc: &Value) -> Result<String, String> {
    serde_json::to_string_pretty(doc).map_err(|e| format!("serialising the JSON document: {e}"))
}

/// The `--help` text describing every document this build emits.
///
/// One string, shown under `--json` on all three commands, because the fields
/// are one contract and a reader should not have to run three commands to see
/// it.
pub const HELP: &str = concat!(
    "Print the run as one JSON document on stdout instead of the report.\n",
    "\n",
    "Field names are stable public API from 0.0.1, versioned by the `schema`\n",
    "field. Adding a field does not move it; removing one or changing what one\n",
    "means does. Only measurements are documents: a usage or I/O error is a\n",
    "line on stderr and a non-zero exit in both modes.\n",
    "\n",
    "scan\n",
    "  schema           JSON contract version (integer)\n",
    "  command          \"scan\"\n",
    "  languages        language name -> tally. One entry per language the\n",
    "                   store holds rows for; a language with no rows has no\n",
    "                   entry, which is not a rate of zero.\n",
    "    resolved            occurrences linked to an in-repository definition\n",
    "    external            occurrences linked outside the repository\n",
    "    local_binding       occurrences a local, parameter or receiver binds\n",
    "    unresolved          occurrences not linked, across every reason\n",
    "    unresolved_reasons  reason name -> count; a stored code this build\n",
    "                        has no name for is `unknown-<code>`\n",
    "    rate                resolved / (resolved + unresolved), or null when\n",
    "                        there is nothing to measure\n",
    "  fqn_collisions   distinct FQNs more than one file declares\n",
    "  file_errors      [{ path, error }] — files the walk reached and could\n",
    "                   not read: no permission, not UTF-8, gone mid-walk, or\n",
    "                   a directory it could not descend into. The scan keeps\n",
    "                   going and measures the rest, so this is how a smaller\n",
    "                   file set than the tree holds becomes visible. One\n",
    "                   entry per file; empty when every file read cleanly.\n",
    "  config           the settings this run read, which decide the file set\n",
    "                   the counts were taken over: { include, exclude, tracks }\n",
    "\n",
    "gate\n",
    "  schema, command (\"gate\"), languages, fqn_collisions, file_errors,\n",
    "  config           as for scan\n",
    "  action           \"compare\" or \"rebase\"\n",
    "  language         the one language this gate measured\n",
    "  baseline_path    the baseline file read or written\n",
    "  corpus, commit   the baseline's provenance; printed, never verified\n",
    "  verdict          \"pass\", \"fail\", \"error\" or \"rebased\"\n",
    "  improved         true when a pass beat its baseline\n",
    "  failures         [{ check, message }]; check is one of rate_regressed,\n",
    "                   local_binding_drift, external_drift\n",
    "  error            why the comparison could not be made, else null\n",
    "  baseline         { resolved, external, local_binding, unresolved }\n",
    "  measured         the same four counts, from this run\n",
    "\n",
    "query\n",
    "  schema, command (\"query\")\n",
    "  verb             \"def\", \"refs\" or \"impact\"\n",
    "  query            the name as it was typed\n",
    "  status           \"ok\", \"no_match\" or \"ambiguous\"\n",
    "  matches          [{ fqn, kind }] when status is not \"ok\"; empty for\n",
    "                   no_match\n",
    "  fqn, kind        the selected node, when status is \"ok\"\n",
    "  shadowed         [{ fqn, kind }] when status is \"ok\": the nodes the\n",
    "                   name also ends, which an exact match won over. Empty\n",
    "                   for almost every query; a non-empty list means the\n",
    "                   answer is one reading of the name and these are the\n",
    "                   others, reachable by spelling more of the name.\n",
    "  def:    declarations [{ file, line }], aliases [{ fqn, kind }]\n",
    "  refs:   rows, occurrences, references [{ file, line, kind, enclosing,\n",
    "          raw_target, count, language, outcome }] where outcome is\n",
    "          { status, package, reason }. `refs` selects the rows that\n",
    "          resolved to the named node, so status is always \"resolved\"\n",
    "          here and package and reason are always null; the three-way\n",
    "          shape is the store's own and is emitted unabridged.\n",
    "  impact: depth, total, truncated, layers [{ depth, nodes }]\n",
    "\n",
    "A stored code this build has no variant for is null, never guessed at.\n",
    "Field names are lowercase_snake; values keep the spelling the text report\n",
    "and the source use (`type-use`, `NeedsTypeInference`).",
);

/// The `--help` text for the configuration file, shown on `scan` and `gate`.
pub const CONFIG_HELP: &str = concat!(
    "Settings are read from `",
    // Kept in step with `config::CONFIG_FILE` by the assertion below.
    "arthron.toml",
    "` at the scanned root, if it is there. Every\n",
    "key is optional; an unknown key is an error naming the key. A `--db` flag\n",
    "wins over the file's `db`.\n",
    "\n",
    "  include = [\"src/**\"]          globs a file must match to be read\n",
    "  exclude = [\"**/vendor/**\"]    globs that keep a file out; wins over include\n",
    "  db      = \"build/graph.redb\"  the graph, relative to the scanned root\n",
    "  [tracks]\n",
    "  java    = false               keep a live language track out of the scan;\n",
    "                                a track this build lacks cannot be switched on",
);

/// One reference row.
fn site(s: &RefSite) -> Value {
    json!({
        "file": s.file,
        "line": s.line,
        "kind": s.kind.map(RefKind::name),
        "enclosing": s.enclosing,
        "raw_target": s.raw_target,
        "count": s.count,
        "language": s.lang.map(Lang::name),
        "outcome": outcome(&s.outcome),
    })
}

/// One stored outcome. All three keys are always present so a reader may take
/// the same path through every row.
fn outcome(o: &StoredOutcome) -> Value {
    match o {
        StoredOutcome::Resolved(_) => json!({
            "status": "resolved", "package": Value::Null, "reason": Value::Null,
        }),
        StoredOutcome::External(package) => json!({
            "status": "external", "package": package, "reason": Value::Null,
        }),
        StoredOutcome::Unresolved(reason) => json!({
            "status": "unresolved", "package": Value::Null, "reason": reason_name(*reason),
        }),
    }
}

/// One node, wherever a document names one.
fn node(m: &Match) -> Value {
    json!({ "fqn": m.name, "kind": kind_name(m.kind) })
}

/// A list of nodes, wherever a document names several.
fn nodes(list: &[Match]) -> Value {
    Value::Array(list.iter().map(node).collect())
}

/// What the scanned root's `arthron.toml` said, as provenance.
///
/// A measurement means nothing without the file set it was taken over, and
/// `include`, `exclude` and `[tracks]` are what decide that set. A gate
/// document already carries `corpus` and `commit`; without these three, a
/// baseline recorded under one configuration and compared under another
/// compares two different repositories and no document can show it. The
/// dangerous shape is partial under-match: excluding the files a language
/// resolves worst makes the rate *improve* and the gate pass.
///
/// All three keys are always present — empty lists and an empty table for a
/// repository with no configuration file — so a reader never has to tell
/// "unset" from "this build did not report it".
fn settings(config: &Config) -> Value {
    json!({
        "include": config.include,
        "exclude": config.exclude,
        "tracks": config.tracks,
    })
}

/// A node kind, as a document spells it.
fn kind_name(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Definition(k) => k.name(),
        NodeKind::Package => "package",
        NodeKind::External => "external",
        NodeKind::Missing => "missing",
    }
}

/// The four gate counts.
fn counts(c: Counts) -> Value {
    json!({
        "resolved": c.resolved,
        "external": c.external,
        "local_binding": c.local_binding,
        "unresolved": c.unresolved,
    })
}

/// The name of the check a failure belongs to.
fn failure(f: &GateFailure) -> Value {
    json!({ "check": f.check(), "message": f.to_string() })
}

/// Per-language tallies, keyed by language name.
fn languages(report: &Report) -> Value {
    let mut out = Map::new();
    for (code, tally) in &report.per_lang {
        // A stored code this build has no variant for still gets an entry:
        // dropping it would hide rows the store is holding.
        let name = Lang::from_code(*code)
            .map_or_else(|| format!("unknown-{code}"), |l| l.name().to_string());
        let unresolved = tally.unresolved_total();
        let mut reasons = Map::new();
        for (reason, count) in &tally.unresolved {
            // A code with no variant becomes `unknown-<code>`, the same shape
            // an unknown language code takes above, and never the bare word
            // `Unknown`: two such codes would collide on one key and the
            // second count would silently replace the first. A map key cannot
            // be null, so the rule "never guess" is kept by carrying the code
            // itself rather than by inventing a name for it.
            let name = match crate::model::reason_from_code(*reason) {
                Some(_) => reason_name(*reason).to_string(),
                None => format!("unknown-{reason}"),
            };
            reasons.insert(name, json!(count));
        }
        out.insert(
            name,
            json!({
                "resolved": tally.resolved,
                "external": tally.external,
                "local_binding": tally.local_binding,
                "unresolved": unresolved,
                "unresolved_reasons": Value::Object(reasons),
                "rate": resolution_rate(tally.resolved, unresolved),
            }),
        );
    }
    Value::Object(out)
}

/// The keys every query document opens with.
fn envelope(verb: &str, query: &str, status: &str) -> Map<String, Value> {
    let mut doc = Map::new();
    doc.insert("schema".to_string(), json!(SCHEMA));
    doc.insert("command".to_string(), json!("query"));
    doc.insert("verb".to_string(), json!(verb));
    doc.insert("query".to_string(), json!(query));
    doc.insert("status".to_string(), json!(status));
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_FILE;

    #[test]
    fn the_config_help_names_the_file_it_documents() {
        assert!(CONFIG_HELP.contains(CONFIG_FILE), "{CONFIG_HELP}");
    }

    #[test]
    fn an_unresolved_outcome_carries_its_reason_and_no_package() {
        assert_eq!(
            outcome(&StoredOutcome::Unresolved(5)),
            json!({
                "status": "unresolved",
                "package": Value::Null,
                "reason": "NeedsTypeInference",
            }),
        );
    }

    #[test]
    fn two_unnamed_reason_codes_do_not_collide_on_one_key() {
        // A store written by a later build can hold reason codes this one has
        // no variant for. Naming them all `Unknown` makes them one map key,
        // and the second count silently replaces the first — a *lost*
        // measurement, in the one document that exists to carry measurements.
        let report = Report {
            per_lang: std::collections::BTreeMap::from([(
                Lang::Go.code(),
                crate::store::LangTally {
                    resolved: 0,
                    external: 0,
                    local_binding: 0,
                    unresolved: std::collections::BTreeMap::from([(200, 3), (201, 5)]),
                },
            )]),
            fqn_collisions: 0,
            file_errors: Vec::new(),
        };
        let reasons = &languages(&report)["go"]["unresolved_reasons"];
        assert_eq!(
            reasons,
            &json!({ "unknown-200": 3, "unknown-201": 5 }),
            "{reasons}",
        );
        // And the total still counts both, so the entry and the sum agree.
        assert_eq!(languages(&report)["go"]["unresolved"], json!(8));
    }

    #[test]
    fn every_gate_check_has_a_name_no_other_check_uses() {
        let names = [
            GateFailure::RateRegressed {
                was: Counts::default(),
                now: Counts::default(),
            }
            .check(),
            GateFailure::LocalBindingDrift { was: 0, now: 1 }.check(),
            GateFailure::ExternalDrift { was: 0, now: 1 }.check(),
        ];
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "{names:?}");
        for name in names {
            assert!(
                HELP.contains(name),
                "`{name}` is emitted but not documented in --help",
            );
        }
    }
}
