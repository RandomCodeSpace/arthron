//! The Java track. **Extractor built; resolver not.** Owns `.java` once it
//! goes live.
//!
//! [`TRACK`] is still registered with `scan: None`, so the driver runs nothing
//! for Java, [`crate::registry::Track::owns_extension`] answers `false` for
//! `java`, and a scan of a tree full of Java sources reads none of them. The
//! seam exists; the resolver does not.
//!
//! # What is here
//!
//! [`JavaLang`] and [`extract`] — one file in, [`crate::model::Definition`]
//! and [`crate::model::Reference`] records out, never an edge. The design
//! contract is the Java case study; every case identifier in this track's
//! comments (`P-01`, `I-04`, `M-05`, `X-02`, …) names a numbered case in it.
//!
//! # Going live
//!
//! Every step below happens inside this file or under `src/track_java/`.
//! Nothing in `pipeline.rs`, `lib.rs`, `model.rs`, `registry.rs` or another
//! track is touched, which is what lets this track and the EcmaScript and
//! Python tracks be built at the same time without conflicting.
//!
//! 1. ~~**Submodules, nested.**~~ Done: [`extract`].
//! 2. ~~**A [`crate::lang::Language`] impl.**~~ Done: [`JavaLang`].
//! 3. ~~**An extractor** implementing [`crate::lang::Extractor`].~~ Done:
//!    [`extract::JavaExtractor`].
//! 4. **A resolver** implementing [`crate::lang::Resolver`]: the one place a
//!    Java [`crate::Outcome`] is produced, and the only layer that links.
//!    Every reference ends `Resolved`, `External`, or `Unresolved(reason)`;
//!    nothing is dropped, and a reference bound by a local, parameter or
//!    catch parameter ends `Unresolved(LocalBinding)`, which is reported
//!    beside `External` and excluded from both terms of the rate. It fills in
//!    [`JavaScope`] and [`JavaConfig`], which are empty until it exists.
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

pub mod extract;

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::registry::Track;

/// Java, as the shared driver sees it.
pub struct JavaLang;

impl Language for JavaLang {
    const LANG: Lang = Lang::Java;
    const DOMAIN: Domain = Domain::Jvm;

    /// [`Lang::Java`]'s own list rather than a second one: two sources of
    /// truth here would mean a walk that reads a file the registry says
    /// nobody owns.
    fn extensions() -> &'static [&'static str] {
        Lang::Java.extensions()
    }

    /// Only directories that are tool state and can never hold a `.java`
    /// file.
    ///
    /// `target/` and `build/` are deliberately **not** here. P-07 wants them
    /// skipped, but G-01 wants `target/generated-sources/**` and
    /// `build/generated/sources/**` read — an annotation processor's output
    /// is real `.java` on disk and the members it declares are named from
    /// hand-written source. `skip_dirs` is a flat list of names and cannot
    /// state an exception, so the build-output rule belongs in the resolver's
    /// `owns_file`, which sees the whole path.
    fn skip_dirs() -> &'static [&'static str] {
        &[".gradle", ".mvn", ".git"]
    }

    type Header = extract::JavaHeader;
    type Scope = JavaScope;
    type Config = JavaConfig;
}

/// The Java resolver's per-file scope.
///
/// Empty until the resolver lands. What goes here is the case study's §9
/// scope chain: compilation unit (five import forms, the implicit
/// `java.lang.*`, same-package) → type (+ supertype closure, member types,
/// enclosing chain) → method → block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JavaScope;

/// Java's project layout: source roots, declared packages, and — optionally —
/// a dependency oracle.
///
/// Empty until the resolver lands. `deps` is optional by design (B-04): Maven
/// POMs are statically parseable, Gradle build scripts are programs and are
/// not, so a Java project may have no `go.mod` equivalent at all and
/// `External` is decided by absence from the indexed definition set rather
/// than by a manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JavaConfig;

/// Java's registration. `scan: None`: the track owns no file and contributes
/// nothing to a scan until the resolver lands.
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

    #[test]
    fn the_language_impl_agrees_with_the_language() {
        assert_eq!(
            <JavaLang as Language>::extensions(),
            Lang::Java.extensions()
        );
        assert_eq!(JavaLang::LANG, Lang::Java);
        assert_eq!(JavaLang::DOMAIN, Domain::Jvm);
        assert_eq!(JavaLang::LANG.domain(), JavaLang::DOMAIN);
        // Build output is not skipped by name: an annotation processor's
        // generated sources live under it and are real declarations (G-01).
        for name in ["target", "build", "out", "bin"] {
            assert!(
                !<JavaLang as Language>::skip_dirs().contains(&name),
                "`{name}` skipped by name would hide generated sources",
            );
        }
    }
}
