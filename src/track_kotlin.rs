//! The Kotlin track. **Live.** Owns `.kt` and `.kts`, at **tier 2**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_kotlin_with`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `kt` and
//! `kts` and the driver runs Kotlin over every one the walk reaches. Three
//! layers:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`lang`] — the constants, the FQN grammar, and the three types only
//!   Kotlin's layers read.
//! - [`resolve`] — the one place a Kotlin [`crate::Outcome`] is produced.
//!
//! There is no `project` layer, and that is a finding rather than an
//! omission: Kotlin writes a declaration's container in the source that
//! declares it. A Gradle build file names artifacts, plugins and source sets
//! and names no package, so nothing outside a `.kt` file decides a Kotlin
//! identity and there is no phase 0 to run.
//!
//! # What tier 2 means here, stated so nobody has to infer it
//!
//! Definitions, structure, and imports. The extractor emits `Definition`
//! records and **import references only** — one per `import` header. It emits
//! no call site, no type use and no supertype, and that is a deliberate
//! refusal rather than an unfinished job: a tier-2 language that emitted them
//! would report a resolution rate that reads like tier-1 coverage while
//! nothing verified the call graph behind it.
//!
//! So Kotlin's gate is an **import-resolution rate**, and it is not
//! comparable with Go's or Java's. Every reference is `Resolved` (it names a
//! package or a declaration in this repository), `External` (no prefix of its
//! path is a package this repository declares), or `Unresolved` with a
//! reason. Nothing is dropped.
//!
//! Several reasons that dominate the tier-1 tracks are *unreachable* here
//! rather than small — `NeedsReceiverType`, `NeedsTypeInference` and
//! `NeedsExpressionType`, because neither a receiver nor an expression is
//! ever named, and `LocalBinding`, because tier 2 emits nothing a block could
//! bind. A Kotlin `local_binding` count of zero is the contract holding, not
//! a bucket nobody filled.
//!
//! # The multiplatform question this track exists to answer
//!
//! okio declares one name in many source sets: 55 `expect` declarations
//! against 492 `actual` ones over 24 source sets, ten of the actuals being
//! `typealias`. The identity space carries **no source-set dimension** — see
//! [`lang`] — so `okio.Lock` is one node with one declaration site per source
//! set, which is what `import okio.Lock` names whichever platform compiles.
//! [`resolve::KtResolver::mergeable`] is where that is decided, and the
//! corpus test is where it is measured on both sides of the store.
//!
//! # Known limits, recorded rather than left to be rediscovered
//!
//! - **The pinned grammar cannot parse a comment or a modified primary
//!   constructor written on the line after a class header.** Six corpus files
//!   hit it — the four `okio/ByteString.kt` source sets and zlib's
//!   `DeflaterSink.kt` and `InflaterSource.kt`. All six lose the class
//!   *body*, which comes back inside a lambda beside the declaration; two of
//!   them lose the declaration as well. The extractor's ancestor allow-list
//!   drops those members rather than emitting them as top-level declarations
//!   of `package okio`, which is the shape a deny-list would have produced —
//!   see [`extract`]. It costs 68 of the corpus's 70
//!   `NoMatchingDefinition` misses, every one of them an
//!   `import okio.ByteString.Companion.…`, and it is a miss rather than a
//!   wrong definition.
//! - **On-demand imports are unexercised.** The corpus writes none.
//! - **A callable node is a callable *name*.** Two overloads are one node
//!   with two declaration sites, because an import names a name and tier 2
//!   emits no site that states an arity. A tier-1 Kotlin track refines this
//!   the way Java's `name/argc` key already does.
//! - **Kotlin is the first live language whose `mergeable` is not
//!   unconditionally `false`, and the shared driver's collision count says
//!   so.** 429 definition identities in the corpus are declared by more than
//!   one file — `expect`/`actual` pairs and overloads across source sets —
//!   and [`crate::pipeline`]'s `mergeable_count` excuses exactly those, so a
//!   cold [`resolve::scan_kotlin`] reports **0** collisions. Two things
//!   follow, and neither gates anything:
//!
//!   1. `arthron gate`/`arthron scan` run every live track and return the
//!      *last* one's report, which recomputes the count from the node table
//!      after Kotlin's subtraction is gone — so the command prints
//!      `fqn collisions 429`, the raw fact rather than the language-endorsed
//!      one.
//!   2. A *warm* scan that re-reads one file holds only that file's
//!      `Definition`s, so a pair split across events cannot be asked and
//!      counts as a collision.
//!
//!   Both are the limitation `mergeable_count`'s own comment predicts — "wrong
//!   for the first [language] that does not [answer `false`]" — and its own
//!   answer is to store enough of the definition to ask, which is a core
//!   change and not a track's to make. The collision count gates nothing and
//!   is not in the baseline, so what it costs today is a printed number that
//!   is coarser than the one this track computes.
//!
//! A baseline is recorded with `arthron gate --rebase`. Kotlin's rate is
//! Kotlin's own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod resolve;

/// Kotlin's registration. **Live**: the track owns `.kt` and `.kts`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for both and the
/// driver runs [`resolve::scan_kotlin_with`] over every Kotlin file the walk
/// reaches.
pub const TRACK: Track = Track {
    name: "kotlin",
    langs: &[Lang::Kotlin],
    scan: Some(resolve::scan_kotlin_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kotlin_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Kotlin]);
        assert!(Lang::Kotlin.owns_extension("kt"));
        assert!(Lang::Kotlin.owns_extension("kts"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("kt"));
        assert!(TRACK.owns_extension("kts"));
        // The extension list registration committed is the one the live track
        // reads: going live widens nothing.
        assert_eq!(Lang::Kotlin.extensions(), ["kt", "kts"]);
        // Kotlin reports one rate, under its own language code, and shares an
        // identity space with nobody — in particular not with Java.
        assert_eq!(Lang::Kotlin.domain(), crate::model::Domain::Kotlin);
        assert_eq!(Lang::Kotlin.tier(), 2);
        assert_eq!(Lang::Kotlin.rate_scope(), "import resolution");
    }
}
