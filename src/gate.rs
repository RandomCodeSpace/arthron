//! The resolution-rate gate: a committed baseline per corpus, and the verdict
//! that decides whether a scan may land.
//!
//! Resolution rate is the primary quality gate, ranked above performance,
//! reported per language and never aggregated. A regression fails the build.
//! This module is the mechanism: [`parse_baseline`] reads a recorded
//! measurement, [`evaluate`] compares a new one against it, and
//! [`render_baseline`] writes a deliberate re-base.
//!
//! **One mechanism, two denominators.** A tier-1 language's rate is taken
//! over calls, type uses and imports; a tier-2 language's track emits no call
//! reference, so its rate is an import-resolution rate over a strictly
//! smaller set — same outcome contract, same ratchet, different measurement
//! (`docs/decisions.md`, 2026-07-27). Which one a number is comes from
//! [`crate::model::Lang::tier`] and is printed on the report line and in
//! `--json`; it is deliberately **not** a baseline field, because the
//! baseline already names its language and a fact stored twice is a fact that
//! can disagree with itself.
//!
//! Three things it deliberately does not do.
//!
//! **It does not store the rate.** The rate is derived from `resolved` and
//! `unresolved` on both sides, so the comparison is exact integer arithmetic
//! and a stored rate can never disagree with its own counts.
//!
//! **It does not compare on `external` or `local_binding` — it fails on any
//! drift in either.** Both categories sit outside *both* terms of the rate, so
//! moving references into them raises the rate without anything being linked.
//! A capability that turns `Unresolved` into `External` re-bases the baseline;
//! it never quietly passes a comparison.
//!
//! **It does not compare the rate alone — it also fails when the rate's
//! denominator shrinks.** A ratio cannot see a reference that stopped being
//! emitted at all: drop one `Resolved` row from a corpus measured at 100% and
//! `resolved / (resolved + unresolved)` is 100% still, and drop an
//! `Unresolved` row from any corpus and the ratio *rises*. Either way an
//! extractor that quietly stopped emitting passes a check named for the one
//! contract it broke. `resolved + unresolved` may therefore grow — new
//! references are how a track improves — and may never fall without a
//! deliberate re-base, which together with the two equality checks above
//! makes "the resolver never drops" a property the gate can actually observe.
//!
//! **It does not verify `corpus` or `commit`.** They are provenance, printed
//! so a wrong baseline is visible in review. A vendored corpus snapshot
//! carries no git metadata to check them against.

use std::fmt;

use crate::resolution_rate;

/// The baseline file format version this build reads and writes.
pub const FORMAT: u32 = 1;

/// The four measured occurrence counts for one language.
///
/// `resolved` and `unresolved` are the two terms of the rate. `external` and
/// `local_binding` are outside both of them and are tracked here precisely
/// because of that: an exclusion nothing watches is a way to move the rate
/// without moving the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// Occurrences linked to an in-repository definition.
    pub resolved: u64,
    /// Occurrences linked to a dependency outside the repository.
    pub external: u64,
    /// Occurrences whose target some enclosing block binds — a local,
    /// parameter, named result or receiver. Policy-caused, not a
    /// language-support failure.
    pub local_binding: u64,
    /// Occurrences that could not be linked, across every reason except
    /// `LocalBinding`.
    pub unresolved: u64,
}

impl Counts {
    /// `resolved + unresolved`: the rate's denominator, widened so the sum
    /// cannot wrap.
    pub fn denominator(self) -> u128 {
        u128::from(self.resolved) + u128::from(self.unresolved)
    }

    /// Every counted occurrence, including the two the rate excludes.
    pub fn total(self) -> u128 {
        self.denominator() + u128::from(self.external) + u128::from(self.local_binding)
    }
}

/// What a scan measured for one language.
pub type Measured = Counts;

/// A recorded measurement that a later scan is compared against.
///
/// `corpus`, `commit` and `language` are written back out verbatim inside
/// double quotes, and the file format carries no escapes, so none of them may
/// contain a `"`, a `\` or a newline. The gate command rejects such a value
/// rather than writing a file it could not read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    /// Baseline file format version. Always [`FORMAT`]; a file declaring any
    /// other version is rejected rather than read under the wrong rules.
    pub format: u32,
    /// The corpus the counts were measured against. Provenance only.
    pub corpus: String,
    /// The commit the counts were measured at. Provenance only.
    pub commit: String,
    /// The language this tally belongs to. Rates are per language and never
    /// aggregated, so a baseline is per language too.
    pub language: String,
    /// The measured counts.
    pub counts: Counts,
}

/// One failing gate check. Every check that fails is reported, not just the
/// first — a run that regressed the rate *and* drifted `external` has two
/// problems, and hiding the second costs a second round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateFailure {
    /// The resolved share of in-repository references fell.
    RateRegressed {
        /// The baseline's counts.
        was: Counts,
        /// This run's counts.
        now: Counts,
    },
    /// The `local_binding` count moved. The anti-gaming tripwire: this bucket
    /// is outside both rate terms, so growing it *raises* the rate while
    /// deleting real edges.
    LocalBindingDrift {
        /// The baseline's count.
        was: u64,
        /// This run's count.
        now: u64,
    },
    /// The `external` count moved. Also outside both rate terms; a capability
    /// that legitimately moves it re-bases the baseline instead of comparing.
    ExternalDrift {
        /// The baseline's count.
        was: u64,
        /// This run's count.
        now: u64,
    },
    /// `resolved + unresolved` fell. References the baseline counted are not
    /// being counted any more, in either term — which the rate itself cannot
    /// see, because a ratio is blind to a row that left the fraction whole.
    DenominatorShrank {
        /// The baseline's `resolved + unresolved`.
        was: u128,
        /// This run's.
        now: u128,
    },
}

impl GateFailure {
    /// The check's stable name, for machine output.
    ///
    /// Separate from [`fmt::Display`], which writes a sentence for a person
    /// and is free to be reworded. This is the string a script branches on,
    /// so it never changes without moving [`crate::json::SCHEMA`].
    pub fn check(&self) -> &'static str {
        match self {
            GateFailure::RateRegressed { .. } => "rate_regressed",
            GateFailure::LocalBindingDrift { .. } => "local_binding_drift",
            GateFailure::ExternalDrift { .. } => "external_drift",
            GateFailure::DenominatorShrank { .. } => "denominator_shrank",
        }
    }
}

impl fmt::Display for GateFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateFailure::RateRegressed { was, now } => write!(
                f,
                "resolution rate regressed: {} ({} / {}) -> {} ({} / {})",
                show_rate(*was),
                was.resolved,
                was.denominator(),
                show_rate(*now),
                now.resolved,
                now.denominator(),
            ),
            GateFailure::LocalBindingDrift { was, now } => write!(
                f,
                "local-binding count drifted: {was} -> {now}; this bucket is outside \
                 both rate terms, so a change here moves the rate without linking \
                 anything — re-base deliberately if it is intended",
            ),
            GateFailure::ExternalDrift { was, now } => write!(
                f,
                "external count drifted: {was} -> {now}; this bucket is outside both \
                 rate terms, so a change here moves the rate without linking \
                 anything — re-base deliberately if it is intended",
            ),
            GateFailure::DenominatorShrank { was, now } => write!(
                f,
                "the rate's denominator shrank: {was} -> {now}; references the \
                 baseline counted are no longer counted in either term, which a \
                 ratio cannot see — every reference is Resolved, External or \
                 Unresolved, so re-base deliberately if fewer is intended",
            ),
        }
    }
}

/// The outcome of comparing one measurement against one baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Nothing regressed.
    Pass {
        /// Whether the measured rate is strictly above the baseline's. An
        /// improvement passes; the ratchet moves up only by a deliberate
        /// re-base commit.
        improved: bool,
    },
    /// At least one check failed.
    Fail(Vec<GateFailure>),
    /// The comparison could not be made at all — which is neither a pass nor
    /// a regression, and must not be reported as either.
    Error(String),
}

/// Compare a measurement against a baseline.
///
/// A zero denominator on either side is an [`GateVerdict::Error`], never a
/// pass: a rate of zero and the absence of any reference at all are different
/// facts, and collapsing them is how a total collapse reads as green.
///
/// The rate comparison is exact rational arithmetic in `u128`
/// (`now.resolved * was_denom` against `was.resolved * now_denom`), never
/// floats. Both products fit: `u64 * u64` cannot overflow `u128`.
pub fn evaluate(baseline: &Baseline, measured: &Measured) -> GateVerdict {
    let was = baseline.counts;
    let now = *measured;

    let was_denom = was.denominator();
    let now_denom = now.denominator();
    if was_denom == 0 {
        return GateVerdict::Error(
            "baseline has nothing to measure: resolved + unresolved is zero".to_string(),
        );
    }
    if now_denom == 0 {
        return GateVerdict::Error(
            "this scan has nothing to measure: resolved + unresolved is zero".to_string(),
        );
    }

    let mut failures = Vec::new();

    let now_share = u128::from(now.resolved) * was_denom;
    let was_share = u128::from(was.resolved) * now_denom;
    if now_share < was_share {
        failures.push(GateFailure::RateRegressed { was, now });
    }
    // The rate is a ratio, and a ratio cannot see a reference that stopped
    // being counted: a corpus at 100% keeps its rate when a `Resolved` row
    // disappears, and any corpus *gains* rate when an `Unresolved` one does.
    // Growth is how a track improves and passes; a fall is a drop and fails.
    if now_denom < was_denom {
        failures.push(GateFailure::DenominatorShrank {
            was: was_denom,
            now: now_denom,
        });
    }
    if was.local_binding != now.local_binding {
        failures.push(GateFailure::LocalBindingDrift {
            was: was.local_binding,
            now: now.local_binding,
        });
    }
    if was.external != now.external {
        failures.push(GateFailure::ExternalDrift {
            was: was.external,
            now: now.external,
        });
    }

    if failures.is_empty() {
        GateVerdict::Pass {
            improved: now_share > was_share,
        }
    } else {
        GateVerdict::Fail(failures)
    }
}

/// The rate as a display string. Presentation only — every comparison in this
/// module is integer arithmetic.
fn show_rate(c: Counts) -> String {
    match resolution_rate(c.resolved, c.unresolved) {
        Some(r) => format!("{:.1}%", r * 100.0),
        None => "n/a".to_string(),
    }
}

/// Parse the baseline file format: flat `key = value` lines, `#` comments,
/// **no tables**.
///
/// It is valid TOML, and a reader this size beats a dependency for six
/// scalars. It is strict on purpose: a table header, an unknown key, a
/// duplicate key, a missing key, a non-numeric or overflowing count, a
/// declared format this build does not know — each is an error. A baseline
/// that silently reads as zeros is worse than no baseline, because every
/// later gate run blesses it.
pub fn parse_baseline(text: &str) -> Result<Baseline, String> {
    let mut format: Option<u32> = None;
    let mut corpus: Option<String> = None;
    let mut commit: Option<String> = None;
    let mut language: Option<String> = None;
    let mut resolved: Option<u64> = None;
    let mut external: Option<u64> = None;
    let mut local_binding: Option<u64> = None;
    let mut unresolved: Option<u64> = None;

    for (i, raw) in text.lines().enumerate() {
        let lineno = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            return Err(format!(
                "line {lineno}: table headers are not part of the baseline format",
            ));
        }
        let (key, rest) = line
            .split_once('=')
            .ok_or_else(|| format!("line {lineno}: expected `key = value`"))?;
        let key = key.trim();
        let rest = rest.trim();
        match key {
            "format" => set_once(&mut format, key, lineno, u32_value(rest, key, lineno)?)?,
            "corpus" => set_once(&mut corpus, key, lineno, string_value(rest, key, lineno)?)?,
            "commit" => set_once(&mut commit, key, lineno, string_value(rest, key, lineno)?)?,
            "language" => set_once(&mut language, key, lineno, string_value(rest, key, lineno)?)?,
            "resolved" => set_once(&mut resolved, key, lineno, u64_value(rest, key, lineno)?)?,
            "external" => set_once(&mut external, key, lineno, u64_value(rest, key, lineno)?)?,
            "local_binding" => set_once(
                &mut local_binding,
                key,
                lineno,
                u64_value(rest, key, lineno)?,
            )?,
            "unresolved" => set_once(&mut unresolved, key, lineno, u64_value(rest, key, lineno)?)?,
            other => return Err(format!("line {lineno}: unknown key `{other}`")),
        }
    }

    let format = required(format, "format")?;
    if format != FORMAT {
        return Err(format!(
            "baseline declares format {format}, this build reads {FORMAT}",
        ));
    }
    Ok(Baseline {
        format,
        corpus: required(corpus, "corpus")?,
        commit: required(commit, "commit")?,
        language: required(language, "language")?,
        counts: Counts {
            resolved: required(resolved, "resolved")?,
            external: required(external, "external")?,
            local_binding: required(local_binding, "local_binding")?,
            unresolved: required(unresolved, "unresolved")?,
        },
    })
}

/// Render a baseline back to the file format, header comment included.
///
/// The header carries the regeneration command, so a baseline recorded from
/// the wrong run — a debug build, a warm store — can be reproduced and
/// checked rather than trusted.
///
/// [`Baseline`]'s provenance strings must contain no `"`, no `\` and no
/// newline; this function writes them verbatim.
pub fn render_baseline(b: &Baseline) -> String {
    format!(
        "# arthron gate baseline — regenerate with:\n\
         #   arthron gate {corpus} --baseline <this file> --rebase --commit <sha>\n\
         # Release build, cold store. Counts are measured, never estimated.\n\
         #\n\
         # `corpus` and `commit` are provenance: printed, never verified.\n\
         # The rate is not stored — it is derived from `resolved` and\n\
         # `unresolved` on both sides, so the comparison is exact integer\n\
         # arithmetic and cannot disagree with its own counts.\n\
         # `external` and `local_binding` sit outside both rate terms; any\n\
         # drift in either fails the gate and must be re-based deliberately.\n\
         format = {format}\n\
         corpus = \"{corpus}\"\n\
         commit = \"{commit}\"\n\
         language = \"{language}\"\n\
         resolved = {resolved}\n\
         external = {external}\n\
         local_binding = {local_binding}\n\
         unresolved = {unresolved}\n",
        format = b.format,
        corpus = b.corpus,
        commit = b.commit,
        language = b.language,
        resolved = b.counts.resolved,
        external = b.counts.external,
        local_binding = b.counts.local_binding,
        unresolved = b.counts.unresolved,
    )
}

/// Whether a provenance string survives [`render_baseline`] and
/// [`parse_baseline`] unchanged.
///
/// The format has no escapes, so a `"` or a newline would produce a file this
/// reader cannot read back — and so would a `\`, which [`string_value`]
/// rejects precisely because escapes do not exist. Checked before writing,
/// never repaired: silently mangling provenance is how a baseline stops
/// meaning what it says.
pub fn is_renderable(value: &str) -> bool {
    !value.contains('"') && !value.contains('\\') && !value.contains('\n') && !value.contains('\r')
}

fn set_once<T>(slot: &mut Option<T>, key: &str, lineno: usize, value: T) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("line {lineno}: duplicate key `{key}`"));
    }
    *slot = Some(value);
    Ok(())
}

fn required<T>(slot: Option<T>, key: &str) -> Result<T, String> {
    slot.ok_or_else(|| format!("missing key `{key}`"))
}

/// The text after a value: blank, or a comment. Anything else is an error
/// rather than something quietly ignored.
fn trailing_ok(tail: &str, key: &str, lineno: usize) -> Result<(), String> {
    let tail = tail.trim();
    if tail.is_empty() || tail.starts_with('#') {
        Ok(())
    } else {
        Err(format!(
            "line {lineno}: unexpected text after `{key}`: `{tail}`",
        ))
    }
}

fn string_value(rest: &str, key: &str, lineno: usize) -> Result<String, String> {
    let body = rest
        .strip_prefix('"')
        .ok_or_else(|| format!("line {lineno}: `{key}` must be a double-quoted string"))?;
    let end = body
        .find('"')
        .ok_or_else(|| format!("line {lineno}: `{key}` has no closing quote"))?;
    let value = &body[..end];
    if value.contains('\\') {
        return Err(format!(
            "line {lineno}: `{key}`: escapes are not part of the baseline format",
        ));
    }
    trailing_ok(&body[end + 1..], key, lineno)?;
    Ok(value.to_string())
}

/// The bare token before any whitespace or comment, with the remainder
/// checked.
fn number_token<'a>(rest: &'a str, key: &str, lineno: usize) -> Result<&'a str, String> {
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '#')
        .unwrap_or(rest.len());
    let (token, tail) = rest.split_at(end);
    trailing_ok(tail, key, lineno)?;
    if token.is_empty() {
        return Err(format!("line {lineno}: `{key}` has no value"));
    }
    Ok(token)
}

fn u64_value(rest: &str, key: &str, lineno: usize) -> Result<u64, String> {
    let token = number_token(rest, key, lineno)?;
    token.parse::<u64>().map_err(|_| {
        format!("line {lineno}: `{key}` must be a non-negative integer below 2^64, not `{token}`",)
    })
}

fn u32_value(rest: &str, key: &str, lineno: usize) -> Result<u32, String> {
    let token = number_token(rest, key, lineno)?;
    token.parse::<u32>().map_err(|_| {
        format!("line {lineno}: `{key}` must be a non-negative integer below 2^32, not `{token}`",)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline(counts: Counts) -> Baseline {
        Baseline {
            format: FORMAT,
            corpus: "corpus/go/codeiq".to_string(),
            commit: "853efde".to_string(),
            language: "go".to_string(),
            counts,
        }
    }

    fn counts(resolved: u64, external: u64, local_binding: u64, unresolved: u64) -> Counts {
        Counts {
            resolved,
            external,
            local_binding,
            unresolved,
        }
    }

    #[test]
    fn identical_counts_pass() {
        let b = baseline(counts(4467, 6085, 12, 5075));
        assert_eq!(
            evaluate(&b, &b.counts),
            GateVerdict::Pass { improved: false },
        );
    }

    #[test]
    fn one_reference_moving_from_resolved_to_unresolved_fails() {
        let b = baseline(counts(4467, 6085, 12, 5075));
        let now = counts(4466, 6085, 12, 5076);
        assert_eq!(
            evaluate(&b, &now),
            GateVerdict::Fail(vec![GateFailure::RateRegressed { was: b.counts, now }]),
        );
    }

    #[test]
    fn an_improvement_passes_and_is_reported_as_improved() {
        let b = baseline(counts(4467, 6085, 12, 5075));
        // One reference moves the other way: the denominator is unchanged, so
        // this is in-repository linking actually improving.
        let now = counts(4468, 6085, 12, 5074);
        assert_eq!(evaluate(&b, &now), GateVerdict::Pass { improved: true });
    }

    #[test]
    fn local_binding_drift_fails_even_when_the_rate_improves() {
        // The tripwire. An over-approximating binding environment moves real
        // references out of `resolved` and into `local_binding`, which is
        // outside both terms — so the rate *rises* while edges are deleted.
        // Rate before: 4467/9542 = 46.8%. After: 4400/9075 = 48.5%.
        //
        // Two checks catch it, and both are the point: the references left
        // the rate's denominator as well as landing in a gated bucket.
        let b = baseline(counts(4467, 6085, 12, 5075));
        let now = counts(4400, 6085, 479, 4675);
        let before = resolution_rate(b.counts.resolved, b.counts.unresolved).unwrap();
        let after = resolution_rate(now.resolved, now.unresolved).unwrap();
        assert!(after > before, "the fixture must make the rate rise");
        assert_eq!(
            evaluate(&b, &now),
            GateVerdict::Fail(vec![
                GateFailure::DenominatorShrank {
                    was: 9542,
                    now: 9075,
                },
                GateFailure::LocalBindingDrift { was: 12, now: 479 },
            ]),
        );
    }

    #[test]
    fn a_dropped_reference_fails_even_at_a_hundred_percent() {
        // The hole a ratio leaves. A tier-2 baseline with nothing unresolved
        // makes `resolved / (resolved + unresolved)` exactly 1, and it stays
        // exactly 1 however many resolved references stop being emitted — so
        // an extractor that quietly dropped an in-repository import would
        // hold "100.0%" while breaking the one contract the rate exists to
        // defend. `external` does not move, so no other check sees it either.
        let b = baseline(counts(53, 36, 0, 0));
        let now = counts(52, 36, 0, 0);
        assert_eq!(
            resolution_rate(b.counts.resolved, b.counts.unresolved),
            resolution_rate(now.resolved, now.unresolved),
            "the fixture must leave the rate untouched",
        );
        assert_eq!(
            evaluate(&b, &now),
            GateVerdict::Fail(vec![GateFailure::DenominatorShrank { was: 53, now: 52 }]),
        );
    }

    #[test]
    fn a_dropped_unresolved_reference_fails_instead_of_reading_as_an_improvement() {
        // The other half, and the one no baseline value can protect against:
        // deleting an `Unresolved` row raises the rate at every baseline.
        // Before: 291/341 = 85.3%. After: 291/340 = 85.6%.
        let b = baseline(counts(291, 1, 0, 50));
        let now = counts(291, 1, 0, 49);
        let before = resolution_rate(b.counts.resolved, b.counts.unresolved).unwrap();
        let after = resolution_rate(now.resolved, now.unresolved).unwrap();
        assert!(after > before, "the fixture must make the rate rise");
        assert_eq!(
            evaluate(&b, &now),
            GateVerdict::Fail(vec![GateFailure::DenominatorShrank { was: 341, now: 340 }]),
        );
    }

    #[test]
    fn a_growing_denominator_is_not_a_drop() {
        // New references are how a track improves. Landing them in `resolved`
        // raises the rate and passes; landing them in `unresolved` lowers it
        // and fails on the rate, never on this check.
        let b = baseline(counts(4467, 6085, 12, 5075));
        assert_eq!(
            evaluate(&b, &counts(4500, 6085, 12, 5075)),
            GateVerdict::Pass { improved: true },
        );
        match evaluate(&b, &counts(4467, 6085, 12, 5100)) {
            GateVerdict::Fail(f) => assert_eq!(
                f.iter().map(GateFailure::check).collect::<Vec<_>>(),
                ["rate_regressed"],
                "{f:?}",
            ),
            other => panic!("expected a rate regression, got {other:?}"),
        }
    }

    #[test]
    fn external_drift_fails() {
        let b = baseline(counts(4467, 6085, 12, 5075));
        let now = counts(4467, 6086, 12, 5075);
        assert_eq!(
            evaluate(&b, &now),
            GateVerdict::Fail(vec![GateFailure::ExternalDrift {
                was: 6085,
                now: 6086,
            }]),
        );
    }

    #[test]
    fn every_failing_check_is_reported_not_only_the_first() {
        let b = baseline(counts(4467, 6085, 12, 5075));
        let now = counts(4466, 6086, 13, 5076);
        match evaluate(&b, &now) {
            GateVerdict::Fail(f) => assert_eq!(f.len(), 3, "{f:?}"),
            other => panic!("expected three failures, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_denominator_is_an_error_not_a_pass() {
        // A corpus with references but nothing to measure is not a rate of
        // zero, and a gate that called it a pass would bless a total
        // collapse.
        let empty = baseline(counts(0, 6085, 12, 0));
        assert!(matches!(
            evaluate(&empty, &counts(1, 6085, 12, 1)),
            GateVerdict::Error(_),
        ));
        let b = baseline(counts(4467, 6085, 12, 5075));
        assert!(matches!(
            evaluate(&b, &counts(0, 6085, 12, 0)),
            GateVerdict::Error(_),
        ));
    }

    #[test]
    fn the_rate_comparison_is_exact() {
        // Both sides round to exactly 1.0 in `f64` — 2^53 + 1 is not
        // representable — so a float comparison passes this. The rationals
        // do not: one reference moved from resolved to unresolved.
        const HUGE: u64 = 9_007_199_254_740_993; // 2^53 + 1
        let b = baseline(counts(HUGE, 0, 0, 0));
        let now = counts(HUGE - 1, 0, 0, 1);
        let was_f = b.counts.resolved as f64 / (b.counts.denominator() as f64);
        let now_f = now.resolved as f64 / (now.denominator() as f64);
        assert_eq!(was_f, now_f, "the fixture must be indistinguishable in f64");
        assert_eq!(
            evaluate(&b, &now),
            GateVerdict::Fail(vec![GateFailure::RateRegressed { was: b.counts, now }]),
        );
    }

    const SAMPLE: &str = "\
# a comment
format = 1
corpus = \"corpus/go/codeiq\"
commit = \"853efde\"
language = \"go\"
resolved = 4467
external = 6085
local_binding = 12
unresolved = 5075
";

    #[test]
    fn parse_reads_every_field() {
        let b = parse_baseline(SAMPLE).expect("parses");
        assert_eq!(b, baseline(counts(4467, 6085, 12, 5075)));
    }

    #[test]
    fn parse_accepts_comments_and_blank_lines_and_surrounding_whitespace() {
        let text = "\
# header

   format = 1   # trailing comment

\tcorpus = \"corpus/go/codeiq\"\t
commit   =   \"853efde\"
language = \"go\"
resolved = 4467
external = 6085
local_binding = 12
unresolved = 5075

# trailing comment line
";
        assert_eq!(
            parse_baseline(text).expect("parses"),
            baseline(counts(4467, 6085, 12, 5075)),
        );
    }

    fn broken(replace: &str, with: &str) -> String {
        SAMPLE.replace(replace, with)
    }

    #[test]
    fn parse_rejects_a_table_header() {
        let text = broken("format = 1", "[counts]\nformat = 1");
        let err = parse_baseline(&text).unwrap_err();
        assert!(err.contains("table headers"), "{err}");
    }

    #[test]
    fn parse_rejects_an_unknown_key() {
        let text = broken("format = 1", "format = 1\nrate = 0.468");
        let err = parse_baseline(&text).unwrap_err();
        assert!(err.contains("unknown key `rate`"), "{err}");
    }

    #[test]
    fn parse_rejects_a_duplicate_key() {
        let text = broken("resolved = 4467", "resolved = 4467\nresolved = 9999");
        let err = parse_baseline(&text).unwrap_err();
        assert!(err.contains("duplicate key `resolved`"), "{err}");
    }

    #[test]
    fn parse_rejects_a_missing_key() {
        let text = broken("local_binding = 12\n", "");
        let err = parse_baseline(&text).unwrap_err();
        assert_eq!(err, "missing key `local_binding`");
    }

    #[test]
    fn parse_rejects_a_non_numeric_count() {
        let text = broken("resolved = 4467", "resolved = \"4467\"");
        let err = parse_baseline(&text).unwrap_err();
        assert!(
            err.contains("`resolved` must be a non-negative integer"),
            "{err}"
        );
    }

    #[test]
    fn parse_rejects_an_overflowing_count() {
        let text = broken("resolved = 4467", "resolved = 18446744073709551616");
        let err = parse_baseline(&text).unwrap_err();
        assert!(err.contains("below 2^64"), "{err}");
    }

    #[test]
    fn parse_rejects_a_negative_count() {
        let text = broken("resolved = 4467", "resolved = -1");
        let err = parse_baseline(&text).unwrap_err();
        assert!(err.contains("non-negative"), "{err}");
    }

    #[test]
    fn parse_rejects_an_unquoted_string() {
        let text = broken("language = \"go\"", "language = go");
        let err = parse_baseline(&text).unwrap_err();
        assert!(err.contains("double-quoted"), "{err}");
    }

    #[test]
    fn parse_rejects_junk_after_a_value() {
        let text = broken("resolved = 4467", "resolved = 4467 4468");
        let err = parse_baseline(&text).unwrap_err();
        assert!(err.contains("unexpected text"), "{err}");
    }

    #[test]
    fn parse_rejects_a_format_this_build_does_not_read() {
        let text = broken("format = 1", "format = 2");
        let err = parse_baseline(&text).unwrap_err();
        assert!(err.contains("declares format 2"), "{err}");
    }

    #[test]
    fn render_then_parse_is_the_identity() {
        let b = baseline(counts(4467, 6085, 12, 5075));
        let rendered = render_baseline(&b);
        assert_eq!(parse_baseline(&rendered).expect("round trips"), b);
        // And rendering the parsed value is byte-identical, so a re-base that
        // changes nothing produces no diff.
        assert_eq!(
            render_baseline(&parse_baseline(&rendered).unwrap()),
            rendered
        );
    }

    #[test]
    fn a_provenance_string_that_cannot_round_trip_is_rejected_not_mangled() {
        assert!(is_renderable("corpus/go/codeiq"));
        assert!(!is_renderable("corpus/\"quoted\""));
        assert!(!is_renderable("two\nlines"));
        // `string_value` rejects `\` — escapes are not part of the format —
        // so a value carrying one would be written and never read back.
        assert!(!is_renderable("corpus\\go\\codeiq"));
    }

    #[test]
    fn counts_totals_do_not_wrap() {
        let c = counts(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        assert_eq!(c.denominator(), 2 * u128::from(u64::MAX));
        assert_eq!(c.total(), 4 * u128::from(u64::MAX));
    }
}
