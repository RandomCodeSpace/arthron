//! The Go track's registration. **Live.**
//!
//! The only live track today, and the shape the three stubs copy. Everything
//! Go-specific — the [`crate::lang::Language`] impl, the extractor, the
//! resolver — stays where it already is; this file is the one line the
//! registry reads.
//!
//! Go's extractor and resolver predate the registry and sit at
//! `src/extract_go.rs` and `src/resolve_go.rs`. A track added from now on
//! nests its own modules under `src/track_<name>/` instead, so that bringing
//! it up adds no `pub mod` line to `lib.rs`. See [`crate::track_java`].

use crate::model::Lang;
use crate::registry::Track;

/// Go's registration.
///
/// `scan` is `Some`, so the driver runs it: one language, one rate, one
/// entry point that is exactly the function `main` used to call directly.
pub const TRACK: Track = Track {
    name: "go",
    langs: &[Lang::Go],
    scan: Some(crate::pipeline::scan_go_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_is_live_and_owns_only_go_files() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Go]);
        assert!(TRACK.owns_extension("go"));
        assert!(!TRACK.owns_extension("java"));
    }
}
