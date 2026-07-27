//! The Rust track. **Live.** Owns `.rs`, at **tier 2**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_rust`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `rs` and the
//! driver runs Rust over every `.rs` file the walk reaches. Four layers, and
//! the boundary between them is the project's first non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`project`] — phase 0: which files are crate roots, which package owns
//!   them, what each package declares as a dependency. Rust states a module's
//!   place in `Cargo.toml` and in the filesystem rather than in the source,
//!   so this is what one `go.mod` line is to Go, spread across ten manifests
//!   and six kinds of target.
//! - [`lang`] — the constants and the three types only Rust's layers read.
//! - [`resolve`] — the one place a Rust [`crate::Outcome`] is produced.
//!
//! # What tier 2 means here, stated so nobody has to infer it
//!
//! Rust is registered at **tier 2**: definitions, structure, and imports. The
//! extractor emits `Definition` records and **import references only** — a
//! `use` leaf, a `mod` declaration, an `extern crate`. It emits no call site
//! and no type use, and that is a deliberate refusal rather than an
//! unfinished job: a tier-2 language that emitted them would report a
//! resolution rate that reads like tier-1 coverage while nothing verified the
//! call graph behind it.
//!
//! So Rust's gate is an **import-resolution rate**, and it is not comparable
//! with Go's or Java's. Every reference is `Resolved` (it names a file,
//! module or definition in this repository), `External` (it names a crate
//! outside it — the sysroot, or a declared dependency), or `Unresolved` with
//! a reason. Nothing is dropped.
//!
//! Two reasons that dominate the tier-1 tracks are *unreachable* here rather
//! than small — [`crate::UnresolvedReason::NeedsReceiverType`] and
//! [`crate::UnresolvedReason::NeedsTypeInference`] — because neither a
//! receiver nor an expression is ever named. So is
//! [`crate::UnresolvedReason::LocalBinding`]: it is the reason a reference to
//! a local carries, and tier 2 emits nothing a block could bind. A Rust
//! `local_binding` count of zero is therefore the contract holding, not a
//! bucket nobody filled.
//!
//! # Known limits, recorded rather than left to be rediscovered
//!
//! - **`#[path = "…"]` is not implemented.** The measured corpus contains
//!   none, so the one mechanism that detaches a module's name from its
//!   conventional file path is unexercised; implementing it blind would be
//!   guessing. A second Rust corpus or a probe is what earns it.
//! - **Editions before 2018 are not modelled.** Every manifest measured is
//!   `edition = "2024"`.
//! - **Glob re-exports enumerate nothing**, and an alias chain is followed no
//!   further than its first hop. Both are stated where they bite, in
//!   [`resolve`].
//! - **Struct fields are not definitions.** Nothing at tier 2 names one.
//!
//! A baseline is recorded with `arthron gate --rebase`. Rust's rate is Rust's
//! own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod project;
pub mod resolve;

/// Rust's registration. **Live**: the track owns `.rs`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for it and the
/// driver runs [`resolve::scan_rust`] over every Rust file the walk reaches.
pub const TRACK: Track = Track {
    name: "rust",
    langs: &[Lang::Rust],
    scan: Some(resolve::scan_rust_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Rust]);
        assert!(Lang::Rust.owns_extension("rs"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("rs"));
        // The extension list registration committed is the one the live track
        // reads: going live widens nothing.
        assert_eq!(Lang::Rust.extensions(), ["rs"]);
        // Rust reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::Rust.domain(), crate::model::Domain::Rust);
    }
}
