//! Local-first code intelligence: parse each file in isolation, then resolve
//! the references between them into a verified graph.
//!
//! **Status: early.** Scanning, the [`gate`] and the [`query`] surface all
//! work, across every tier-1 language, in a report for a person, a [`json`]
//! document for a script, or an [`mcp`] tool call for an agent; the watch
//! surface is not built yet. The API is pre-1.0 and will change.
//!
//! # The contract
//!
//! [`Outcome`] is the type the whole engine is built around. Every reference
//! between two files resolves to exactly one of its three variants, and there
//! is no way to express "dropped".
//!
//! A file-local extractor cannot know whether the symbol it just saw is defined
//! elsewhere in the repository. Let it emit edges anyway and it must either
//! guess — leaving something downstream to discard the guess — or give up and
//! emit nothing. Both yield a graph that looks populated, links almost nothing
//! across files, and reports success while doing it.
//!
//! So extractors emit references and never edges; one resolver, which sees
//! every file, owns all linking; and a reference that cannot be linked becomes
//! [`Outcome::Unresolved`] carrying a [`UnresolvedReason`] rather than
//! vanishing. Aggregated, those reasons are the measurement of where language
//! support is thin — which is what [`resolution_rate`] reports.

#![forbid(unsafe_code)]

/// Why a reference could not be linked to a definition.
///
/// Every unresolved reference carries one of these. They are the signal for
/// where language support is thin — aggregated, they drive the resolution-rate
/// quality gate that ranks above performance in this project.
/// The variants are ordered by their stable storage code, which
/// [`model::reason_code`] fixes and which is never renumbered. Adding a
/// reason appends a code; a reason earns its own variant only when its fix
/// is a different piece of work from every existing variant's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UnresolvedReason {
    /// The call target is chosen at runtime; no static target exists.
    DynamicDispatch,
    /// The definition is produced by a generator not expanded here — a macro,
    /// an annotation processor, a metaclass, or a codegen step.
    Generated,
    /// The target names a package outside the repository that was not indexed.
    UnknownPackage,
    /// The language is supported structurally, but not for call-graph resolution.
    ///
    /// Coverage spans every language the extraction layer parses; full
    /// resolution is tiered, and this reason marks the difference honestly
    /// rather than inventing an edge.
    TierTwoLanguage,
    /// The reference was understood, the lookup table was complete, and the
    /// name is absent. In a corpus that compiles this should mean *our* bug,
    /// and should sit near zero.
    NoMatchingDefinition,
    /// The receiver is a name with no declared or annotated type, so finding
    /// the target needs type inference this tool does not yet perform.
    NeedsTypeInference,
    /// The target is bound by a local, parameter, named result, receiver,
    /// catch parameter, or closure variable — by design not a node.
    ///
    /// Policy-caused, not a language-support failure: reported on its own
    /// line beside `External` and excluded from both terms of the resolution
    /// rate.
    ///
    /// # The root-binding rule
    ///
    /// One rule, and every track reads it the same way. A reference is a
    /// `LocalBinding` **iff** its target root is a name
    /// ([`model::TargetRoot::Name`]) and, at the reference's site, some
    /// enclosing binding environment that is **not a node** binds
    /// `target.segments[0]` in the declaration space that segment is looked up
    /// in.
    ///
    /// A **non-node binding environment** is every scope strictly inside a
    /// nameable definition: function, method, constructor, lambda, closure and
    /// comprehension bodies and their parameter lists; receivers; named
    /// results; catch and `except` parameters; pattern and `instanceof`
    /// variables; type parameters; and any block nested in one. Plus the type
    /// declarations a language gives no canonical name — JLS §6.7 local and
    /// anonymous classes, a Go `type` inside a function body.
    ///
    /// It is **not** the module, package or namespace top level, nor a class,
    /// struct, interface or object body. What those bind *is* a node, and
    /// calling one local would delete a real reference from both rate terms.
    ///
    /// **Depth is irrelevant.** `x`, `x.y` and `x.y.z` are all `LocalBinding`
    /// when `x` is bound locally: nothing outside the block can name the thing
    /// a member of a local hangs off, so the member is no more a node than the
    /// local is.
    ///
    /// **`this`, `super` and expression roots are never `LocalBinding`** — no
    /// name is looked up, so no binding can shadow one. **A field is never
    /// local**: a field is a node.
    ///
    /// Java has one further producer, and it is not a question about a bound
    /// *name*: a member found on the frame of a class §6.7 gives no canonical
    /// name is `LocalBinding` too, because the type hosting it is not a node.
    /// It shares the line because it shares the reason.
    ///
    /// # Why uniform
    ///
    /// Because the rate is compared across languages. This was implemented
    /// three different ways — Go and ECMA broadly, Java and Python only when
    /// the *whole* target was the bound name, and structurally never in the
    /// tier-2 tracks — so `f.m()` sat outside both rate terms in Go and inside
    /// them in Java, and the two numbers were computed over differently-sized
    /// denominators. A per-language rate that is not measured over the same
    /// surface is not a comparison, and the gate exists to be compared.
    ///
    /// Visibility stays per language: Go's end-of-declaration position rule,
    /// ECMA hoisting, Python's "position never matters", Java's §6.3 ranges
    /// and §6.5.1 MethodName carve-out are each that language's own. The rule
    /// above is uniform about which *positions* bind, not about how a track
    /// computes scope.
    ///
    /// The 14 tier-2 tracks emit `Import` references only, and every one of
    /// them reports zero here. That zero is load-bearing — it is what makes a
    /// tier-2 rate impossible to raise by reclassification — and this rule is
    /// a no-op there rather than an exemption from it.
    LocalBinding,
    /// `x.M()` where `x` has a declared or annotated type stated in this file
    /// and that type is in the repository. Declared-type lookup, not inference.
    NeedsReceiverType,
    /// The selector's operand is an expression rather than a name:
    /// `f().M()`, `m[k].M()`.
    NeedsExpressionType,
    /// The receiver type is known and in-repository, the member is in no
    /// indexed supertype, and at least one supertype is external or unindexed.
    UnindexedSupertype,
    /// Two sources supply one name and the language calls the result ambiguous.
    AmbiguousExport,
    /// An on-demand or star import whose source's export set could not be
    /// enumerated.
    WildcardImport,
    /// The module path is not a literal.
    DynamicModuleSpecifier,
    /// The specifier is a literal and resolved to no module under the
    /// configured resolution.
    ModuleNotFound,
    /// The container exists and the member exists, but the container does not
    /// export it.
    NotExported,
    /// Owner and member name resolved; two or more declarations are applicable
    /// at the site's arity, or the discriminating type is unavailable.
    AmbiguousOverload,
    /// A multi-segment qualifier whose container/type/member split could not
    /// be determined.
    AmbiguousName,
    /// The project's layout could not be determined, so the failure is
    /// arthron's own inference rather than a missing definition.
    ProjectLayoutUnknown,
    /// An alias chain re-entered a key it had already visited, or ran past the
    /// hop ceiling, so the walk was cut.
    ///
    /// A distinct reason from `NoMatchingDefinition` on purpose: the lookup
    /// table was *not* complete — the walk stopped itself — and saying the
    /// name is absent would blame the corpus for a bound this resolver chose.
    AliasCycle,
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
/// `resolved / (resolved + unresolved)`. Two categories are excluded from
/// **both** terms, because neither is a success or a failure of
/// in-repository linking:
///
/// - `External` — a link to a dependency outside the repository.
/// - [`UnresolvedReason::LocalBinding`] — a reference to a name some
///   enclosing block binds. Locals are not nodes by design, so this bucket
///   is policy-caused rather than a gap in language support. It is reported
///   on its own line beside `External` and never counted as unresolved;
///   folding it into the denominator would fill the gate with a category
///   nothing can ever fix.
///
/// Excluding a category from both terms is also how a rate can rise without
/// anything improving, which is why the gate tracks the `LocalBinding` and
/// `External` counts themselves and fails on drift in either.
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

pub mod config;
pub mod extract_go;
pub mod gate;
pub mod json;
pub mod lang;
pub mod mcp;
pub mod model;
pub mod pipeline;
pub mod query;
pub mod registry;
pub mod resolve_go;
pub mod sg;
pub mod store;

// One module per language track. Every registered track is declared here,
// live or not, so that bringing a language up edits only that track's own
// file — see `registry.rs` for that rule, and `track_go` for the shape a live
// track takes. A track nests its extractor and resolver under
// `src/track_<name>/`, which is why this list does not grow when one is built.
//
// Both groups below are alphabetical, which the committed order is not: that
// order is `REGISTRY`'s and `Lang::ALL`'s (go, java, ecma, python, then the
// tier-2 tracks), and nothing reads the order of these declarations.
//
// The four tier-1 tracks.
pub mod track_ecma;
pub mod track_go;
pub mod track_java;
pub mod track_python;

// The ratified tier-2 tracks. All disabled: each carries `scan: None`, owns
// no file, and reads nothing.
pub mod track_bash;
pub mod track_cpp;
pub mod track_csharp;
pub mod track_dart;
pub mod track_elixir;
pub mod track_haskell;
pub mod track_hcl;
pub mod track_kotlin;
pub mod track_lua;
pub mod track_php;
pub mod track_ruby;
pub mod track_rust;
pub mod track_scala;
pub mod track_swift;

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
