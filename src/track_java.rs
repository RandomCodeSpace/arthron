//! The Java track. **Not live.** Owns `.java`.
//!
//! [`TRACK`] is registered with `scan: None`, so the driver runs nothing for
//! Java, [`crate::registry::Track::owns_extension`] answers `false` for
//! `java`, and a scan of a tree full of Java sources reads none of them. The
//! seam exists; the work does not.
//!
//! # Going live
//!
//! Every step below happens inside this file or under `src/track_java/`.
//! Nothing in `pipeline.rs`, `lib.rs`, `model.rs`, `registry.rs` or another
//! track is touched, which is what lets this track and the EcmaScript and
//! Python tracks be built at the same time without conflicting.
//!
//! 1. **Submodules, nested.** Declare them here — `mod extract;`,
//!    `mod resolve;` — so they resolve to `src/track_java/extract.rs` and
//!    `src/track_java/resolve.rs`. `lib.rs` already declares `track_java`, so
//!    it needs no new `pub mod` line.
//! 2. **A [`crate::lang::Language`] impl**, say `JavaLang`, with
//!    `const LANG = Lang::Java`, `const DOMAIN = Domain::Jvm`,
//!    `extensions()` returning [`Lang::extensions`] for `Lang::Java` rather
//!    than a second list, `skip_dirs()` for build output (`target`, `build`,
//!    `out`), and the three associated types (`Header`, `Scope`, `Config`)
//!    that only this track's own layers may read.
//! 3. **An extractor** implementing [`crate::lang::Extractor`], parsing with
//!    [`crate::sg::SourceTree::parse_java`]. It receives one path and one
//!    source string and it emits [`crate::model::Definition`] and
//!    [`crate::model::Reference`] records — **never an edge**. It has no
//!    probe and no config, so it has nothing to link against even by
//!    accident.
//! 4. **A resolver** implementing [`crate::lang::Resolver`]: the one place a
//!    Java [`crate::Outcome`] is produced, and the only layer that links.
//!    Every reference ends `Resolved`, `External`, or `Unresolved(reason)`;
//!    nothing is dropped, and a reference bound by a local, parameter or
//!    catch parameter ends `Unresolved(LocalBinding)`, which is reported
//!    beside `External` and excluded from both terms of the rate.
//! 5. **Honest reasons.** Java's floor is real: a call on a receiver whose
//!    type is not stated in the file is
//!    [`crate::UnresolvedReason::NeedsReceiverType`] or
//!    [`crate::UnresolvedReason::NeedsTypeInference`], and a large such floor
//!    is the correct first measurement. It is not to be moved into
//!    `LocalBinding` or `External`, both of which leave the rate's
//!    denominator and would raise the number without linking anything.
//! 6. **An entry point** with the shape of [`crate::registry::TrackScan`]:
//!    `fn scan_java(root, db) -> Result<Report, String>`, whose body is
//!    `crate::pipeline::scan::<JavaLang>(root, db, &JavaExtractor, &JavaResolver)`.
//! 7. **Flip the switch here**: `scan: None` becomes `scan: Some(scan_java)`.
//!    That single edit is what enables the language.
//! 8. **A baseline.** Record `baselines/<corpus>.txt` with `arthron gate
//!    --rebase` and let the ratchet hold it. The rate is Java's own — it is
//!    never added to Go's, and no combined number is ever reported.
//!
//! Two tracks live at once share one store. That is safe because a scan
//! forgets only files carrying an extension the running track owns, and
//! extension ownership is a partition (see [`Lang::for_extension`]); Java's
//! rows survive a Go scan and Go's survive a Java one.

use crate::model::Lang;
use crate::registry::Track;

/// Java's registration. `scan: None`: the track owns no file and contributes
/// nothing to a scan until the work above lands.
pub const TRACK: Track = Track {
    name: "java",
    langs: &[Lang::Java],
    scan: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_is_registered_but_not_live() {
        assert!(!TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Java]);
        // The language owns `.java`; the disabled track does not, so no walk
        // reads one.
        assert!(Lang::Java.owns_extension("java"));
        assert!(!TRACK.owns_extension("java"));
    }
}
