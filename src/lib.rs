//! Local-first code intelligence.
//!
//! **Status: design phase.** The engine is not implemented. What ships here is
//! the one thing the design is built around — the contract that every reference
//! between two files resolves to exactly one outcome, and that failing to
//! resolve is recorded rather than discarded.
//!
//! The approved design lives in the repository under
//! `docs/superpowers/specs/`. Read that before depending on anything here.
//!
//! # Why this contract exists
//!
//! Its predecessor let each of 100+ single-file detectors build graph edges
//! directly, then silently dropped any edge whose endpoints were not already
//! known. Measured on a 1.33M-line corpus, that produced 14,423 method nodes
//! and exactly one call edge, with zero edges reaching resolved confidence —
//! and reported success throughout.
//!
//! A detector sees one file. It cannot know whether a target exists elsewhere,
//! so it either guesses (and the guess is dropped) or gives up (and emits
//! nothing). Both paths yield a graph that looks populated and links nothing.
//!
//! The fix is to make the failure impossible to hide: detectors emit
//! references, one resolver owns linking, and [`Outcome::Unresolved`] carries a
//! [`UnresolvedReason`] instead of vanishing.

#![forbid(unsafe_code)]

/// Why a reference could not be linked to a definition.
///
/// Every unresolved reference carries one of these. They are the signal for
/// where language support is thin — aggregated, they drive the resolution-rate
/// quality gate that ranks above performance in this project.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UnresolvedReason {
    /// The call target is chosen at runtime; no static target exists.
    DynamicDispatch,
    /// The definition is produced by a macro or code generator not expanded here.
    MacroGenerated,
    /// The target can only be found by inferring the type of an expression
    /// (for example a method call on a variable), and this tool does not yet
    /// perform type inference for the language.
    NeedsTypeInference,
    /// The target names a package outside the repository that was not indexed.
    UnknownPackage,
    /// The language is supported structurally, but not for call-graph resolution.
    ///
    /// Coverage spans every language the extraction layer parses; full
    /// resolution is tiered, and this reason marks the difference honestly
    /// rather than inventing an edge.
    TierTwoLanguage,
    /// The reference was understood but matched no definition anywhere.
    NoMatchingDefinition,
}

/// The result of resolving a single reference.
///
/// There is no fourth variant, and in particular there is no way to express
/// "dropped". That is the point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<Id, Package> {
    /// Linked to a definition inside this repository. Verified.
    Resolved(Id),
    /// Linked to a known dependency outside this repository.
    External(Package),
    /// Could not be linked. Recorded with a reason, never discarded.
    Unresolved(UnresolvedReason),
}

impl<Id, Package> Outcome<Id, Package> {
    /// Whether this reference was linked to a definition in this repository.
    ///
    /// Only [`Outcome::Resolved`] counts. An external link is a real edge but
    /// not evidence that in-repository resolution is working, so it is excluded
    /// from the numerator and the denominator of [`resolution_rate`].
    pub fn is_resolved(&self) -> bool {
        matches!(self, Outcome::Resolved(_))
    }

    /// Why this reference failed to resolve, if it did.
    pub fn unresolved_reason(&self) -> Option<&UnresolvedReason> {
        match self {
            Outcome::Unresolved(reason) => Some(reason),
            _ => None,
        }
    }
}

/// The share of in-repository references that were actually linked.
///
/// `resolved / (resolved + unresolved)`. External references are excluded from
/// both terms — they are neither a success nor a failure of in-repository
/// linking.
///
/// Returns `None` when there is nothing to measure, because a rate of zero and
/// the absence of any reference at all are different facts and collapsing them
/// is how a regression hides.
///
/// This is computed per language and never aggregated across languages: one
/// combined number lets a collapse in one language be masked by another.
pub fn resolution_rate(resolved: u64, unresolved: u64) -> Option<f64> {
    let total = resolved.checked_add(unresolved)?;
    if total == 0 {
        return None;
    }
    Some(resolved as f64 / total as f64)
}

pub mod extract_go;
pub mod model;
pub mod pipeline;
pub mod resolve_go;
pub mod sg;
pub mod store;

#[cfg(test)]
mod tests {
    use super::*;

    type TestOutcome = Outcome<u32, String>;

    #[test]
    fn only_resolved_counts_as_resolved() {
        let resolved: TestOutcome = Outcome::Resolved(7);
        let external: TestOutcome = Outcome::External("serde".to_owned());
        let unresolved: TestOutcome = Outcome::Unresolved(UnresolvedReason::DynamicDispatch);

        assert!(resolved.is_resolved());
        assert!(!external.is_resolved());
        assert!(!unresolved.is_resolved());
    }

    #[test]
    fn unresolved_carries_its_reason() {
        let outcome: TestOutcome = Outcome::Unresolved(UnresolvedReason::TierTwoLanguage);
        assert_eq!(
            outcome.unresolved_reason(),
            Some(&UnresolvedReason::TierTwoLanguage)
        );

        let resolved: TestOutcome = Outcome::Resolved(1);
        assert_eq!(resolved.unresolved_reason(), None);
    }

    #[test]
    fn rate_is_the_resolved_share() {
        assert_eq!(resolution_rate(3, 1), Some(0.75));
        assert_eq!(resolution_rate(1, 0), Some(1.0));
    }

    #[test]
    fn the_inherited_baseline_is_zero_not_absent() {
        // 14,423 method nodes, 1 call edge, 0 resolved. Zero is a measurement.
        assert_eq!(resolution_rate(0, 14_423), Some(0.0));
    }

    #[test]
    fn nothing_to_measure_is_not_a_rate_of_zero() {
        assert_eq!(resolution_rate(0, 0), None);
    }

    #[test]
    fn saturating_input_does_not_panic() {
        assert_eq!(resolution_rate(u64::MAX, 1), None);
    }

    #[test]
    fn needs_type_inference_is_a_reason() {
        let outcome: TestOutcome = Outcome::Unresolved(UnresolvedReason::NeedsTypeInference);
        assert_eq!(
            outcome.unresolved_reason(),
            Some(&UnresolvedReason::NeedsTypeInference)
        );
    }
}
