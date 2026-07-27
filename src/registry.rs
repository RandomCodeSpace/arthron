//! The language registry: every track this build knows about, in one fixed
//! order, so the driver iterates rather than naming a language.
//!
//! A **track** is one language team's work — its extractor, its resolver, its
//! [`crate::lang::Language`] impl — plus the one thing the driver needs from
//! it: an entry point. A track is not a language. [`crate::track_ecma`] owns
//! two languages, because JavaScript and TypeScript share an identity space
//! and a resolver family; they still report two rates, never one.
//!
//! # The zero-conflict rule
//!
//! Every entry in [`REGISTRY`] is already listed, live or not. Bringing a
//! language up therefore edits **only that track's own module**: `TRACK.scan`
//! goes from `None` to `Some(...)`. Nothing in `pipeline.rs`, `lib.rs`,
//! `model.rs`, `registry.rs` or any other shared file moves, so two language
//! tracks developed in parallel cannot conflict with each other and neither
//! can conflict with the core.
//!
//! That is why `scan` is an `Option` on a per-track const rather than a
//! `#[cfg]`, a feature flag, or a list the enabling commit appends to: all
//! three put the switch in a file every track shares.

use std::path::Path;

use crate::model::Lang;
use crate::store::Report;

/// A track's scan entry point: `(repository root, database path)`.
///
/// Deliberately the same signature the driver already had for Go, so a track
/// going live is a function pointer and not a new abstraction.
pub type TrackScan = fn(&Path, &Path) -> Result<Report, String>;

/// One language track's registration.
pub struct Track {
    /// The track's module name — `"go"`, `"java"`, `"ecma"`, `"python"`.
    /// Diagnostics and tests only; nothing is keyed off it.
    pub name: &'static str,
    /// The languages this track reports, in committed code order.
    ///
    /// A list, not a single language, because one resolver family can own
    /// several. Each still gets its own line in the report and its own rate:
    /// the registry never aggregates, and neither does anything reading it.
    pub langs: &'static [Lang],
    /// The track's entry point, or `None` while the track is not live.
    ///
    /// `None` is the whole of "disabled": the driver runs nothing, and
    /// [`Track::owns_extension`] answers `false` for every extension, so the
    /// track's languages own no file even though [`Lang`] knows their
    /// extensions.
    pub scan: Option<TrackScan>,
}

impl Track {
    /// Whether this track contributes to a scan at all.
    pub const fn is_enabled(&self) -> bool {
        self.scan.is_some()
    }

    /// Whether a file with this extension (no dot) belongs to this track.
    ///
    /// A disabled track owns no extension. Extension *ownership* is a
    /// property of the language ([`Lang::owns_extension`]) and stays true
    /// whether or not anything is built for it; whether a scan reads such a
    /// file is a property of the track, and this is that question.
    pub fn owns_extension(&self, ext: &str) -> bool {
        self.is_enabled() && self.langs.iter().any(|l| l.owns_extension(ext))
    }
}

/// Every track, in the order the driver runs them.
///
/// The order is a committed fact, not an incidental one: it is the order
/// tracks write to the store within a single scan, and therefore the order
/// their reports are produced in. It follows [`Lang::ALL`], so appending a
/// language appends a track.
pub static REGISTRY: &[Track] = &[
    crate::track_go::TRACK,
    crate::track_java::TRACK,
    crate::track_ecma::TRACK,
    crate::track_python::TRACK,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Language;
    use crate::resolve_go::GoLang;

    #[test]
    fn iteration_order_is_deterministic_and_follows_lang_all() {
        let names: Vec<&str> = REGISTRY.iter().map(|t| t.name).collect();
        assert_eq!(names, ["go", "java", "ecma", "python"]);
        // Iterating again yields the same sequence: `REGISTRY` is a static
        // slice, not a set or a map whose order an implementation may choose.
        let again: Vec<&str> = REGISTRY.iter().map(|t| t.name).collect();
        assert_eq!(names, again);

        // Every language belongs to exactly one track, and the tracks list
        // them in `Lang::ALL` order.
        let langs: Vec<Lang> = REGISTRY.iter().flat_map(|t| t.langs).copied().collect();
        assert_eq!(langs, Lang::ALL);
    }

    #[test]
    fn go_is_the_only_live_track() {
        let live: Vec<&str> = REGISTRY
            .iter()
            .filter(|t| t.is_enabled())
            .map(|t| t.name)
            .collect();
        assert_eq!(live, ["go"]);
    }

    #[test]
    fn a_disabled_track_owns_no_extension() {
        for track in REGISTRY.iter().filter(|t| !t.is_enabled()) {
            for lang in track.langs {
                for ext in lang.extensions() {
                    // The language still owns it — that fact is committed
                    // ahead of the implementation — but the track does not,
                    // so no walk reads the file.
                    assert!(lang.owns_extension(ext));
                    assert!(
                        !track.owns_extension(ext),
                        "disabled track `{}` claims `.{ext}`",
                        track.name,
                    );
                }
            }
        }
    }

    #[test]
    fn the_live_track_owns_exactly_its_languages_extensions() {
        let go = REGISTRY.iter().find(|t| t.name == "go").expect("go track");
        assert!(go.owns_extension("go"));
        for ext in ["java", "js", "mjs", "cjs", "ts", "py", "rs"] {
            assert!(!go.owns_extension(ext), "go track claims `.{ext}`");
        }
        // The registry's view of what Go owns and the `Language` impl's view
        // are the same list. Two sources of truth here would mean a walk that
        // reads a file the registry says nobody owns.
        assert_eq!(Lang::Go.extensions(), <GoLang as Language>::extensions());
    }
}
