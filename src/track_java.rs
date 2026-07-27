//! The Java track. **Live.** Owns `.java`.
//!
//! [`TRACK`] is registered with `scan: Some(scan_java)`, so the driver runs
//! Java, [`crate::registry::Track::owns_extension`] answers `true` for `java`,
//! and a scan of a tree of Java sources reads every one of them.
//!
//! # What is here
//!
//! [`JavaLang`], [`extract`] and [`resolve`] — one file in,
//! [`crate::model::Definition`] and [`crate::model::Reference`] records out,
//! never an edge; then one resolver that owns every link. The design contract
//! is the Java case study; every case identifier in this track's comments
//! (`P-01`, `I-04`, `M-05`, `X-02`, …) names a numbered case in it.
//!
//! # The three layers
//!
//! 1. [`fqn`] — the FQN grammar, the one place a Java identity string is
//!    built. Both the extractor (for overload grouping) and the resolver (for
//!    every node name and every candidate) read it, so a definition's identity
//!    and the candidate a reference probes for it cannot drift apart.
//! 2. [`extract`] — [`extract::JavaExtractor`], one file in, records out. It
//!    is handed a path and a source string and nothing else, so it has nothing
//!    it could link against.
//! 3. [`resolve`] — [`resolve::JavaResolver`], the only place a Java
//!    [`crate::Outcome`] is produced. Every reference ends `Resolved`,
//!    `External`, or `Unresolved(reason)`; nothing is dropped, and a reference
//!    whose *type* name is bound by a type parameter or a local class ends
//!    `Unresolved(LocalBinding)`, which is reported beside `External` and
//!    excluded from both terms of the rate.
//!
//! # Honest reasons
//!
//! Java's floor is real: a call on a receiver whose type is not stated in the
//! file is [`crate::UnresolvedReason::NeedsTypeInference`] — and never
//! [`crate::UnresolvedReason::NeedsReceiverType`], whose definition is the
//! opposite case, the one where the type *is* stated and *is* in the
//! repository, which this resolver looks up rather than reports (X-02). A
//! member of an in-repository type that no indexed supertype declares is
//! [`crate::UnresolvedReason::UnindexedSupertype`], and a large such floor is
//! the correct first measurement. None of it is to be moved into
//! `LocalBinding` or `External`, both of which leave the rate's denominator
//! and would raise the number without linking anything.
//!
//! Two tracks live at once share one store. That is safe because a scan
//! forgets only files carrying an extension the running track owns, and
//! extension ownership is a partition (see [`Lang::for_extension`]); Java's
//! rows survive a Go scan and Go's survive a Java one. The manifest fence is
//! per-language and Java's config digest is empty, so Java never invalidates
//! anything.

pub mod extract;
pub mod fqn;
pub mod resolve;

use std::path::Path;

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::registry::Track;
use crate::store::Report;

pub use resolve::{JavaConfig, JavaResolver, JavaScope};

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
    /// [`crate::lang::Resolver::owns_file`], which sees the whole path.
    fn skip_dirs() -> &'static [&'static str] {
        &[".gradle", ".mvn", ".git"]
    }

    type Header = extract::JavaHeader;
    type Scope = JavaScope;
    type Config = JavaConfig;
}

/// Scan a repository's Java. The entry point [`TRACK`] holds.
///
/// The body is the shared driver, instantiated: nothing Java-specific happens
/// here, because everything Java-specific is behind the two trait objects.
pub fn scan_java(root: &Path, db_path: &Path) -> Result<Report, String> {
    crate::pipeline::scan::<JavaLang>(
        root,
        db_path,
        &extract::JavaExtractor,
        &resolve::JavaResolver,
    )
}

/// Java's registration. Live: the track owns `.java` and contributes its own
/// rate, which is never added to Go's.
pub const TRACK: Track = Track {
    name: "java",
    langs: &[Lang::Java],
    scan: Some(scan_java),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_is_live_and_owns_only_java_files() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Java]);
        assert!(Lang::Java.owns_extension("java"));
        assert!(TRACK.owns_extension("java"));
        // Extension ownership is a partition: the Java track never reads a
        // file another track owns, which is what lets both be live at once.
        assert!(!TRACK.owns_extension("go"));
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
