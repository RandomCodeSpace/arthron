//! The HCL track. **Live.** Owns `.tf`, at **tier 2, best effort**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_hcl_with`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `tf` and the
//! driver runs HCL over every `.tf` file the walk reaches. Going live edited
//! this file and this track's own modules, which is the whole of the
//! registry's zero-conflict rule — see [`crate::registry`] for why, and
//! [`crate::track_go`] for the shape a live track takes.
//!
//! Three layers, and the boundary between them is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`lang`] — the constants, and the FQN grammar the other two agree on.
//! - [`resolve`] — the one place an HCL [`crate::Outcome`] is produced.
//!
//! # What "best effort" means, stated so nobody has to infer it
//!
//! It is a statement about how much of the language this track reads, and
//! **not** about how honestly it reports what it read. Every reference here
//! is `Resolved`, `External`, or `Unresolved` with a reason from the ratified
//! taxonomy — none was added for HCL — the resolver never drops, and a low
//! rate stated plainly beats a high one constructed cleverly.
//!
//! # HCL has no import statement, and the unit of resolution is a directory
//!
//! This is the whole shape of the track, and it is why the corpus was chosen:
//!
//! - A Terraform **module is a directory**. Every `.tf` file in it
//!   contributes to one namespace by position in the filesystem alone —
//!   there is no `package` clause, no qualifier and no path anywhere in the
//!   source. So a file's container is its directory, declared by every file
//!   under it exactly as a Go package is declared by every file in its own.
//! - The **only import-like site is a `module` block's `source`**, and what
//!   it names is a directory rather than a file: the reference binds to the
//!   container, which stands for every `.tf` file in the target at once.
//! - Everything else is expression-level. `var.x`, `local.y`,
//!   `module.m.out`, `aws_vpc.this.id` — the corpus writes 750, 191, 1,188
//!   and more of them — and all are out of tier-2 scope. They are not
//!   emitted at all, so they enter no denominator this track cannot answer
//!   for.
//!
//! # The denominator is small, and that is the measurement
//!
//! Sixty-five files, thousands of declarations, and **24 references** — one
//! per `module` block. That is not a thin extractor; it is what a language
//! with no import statement looks like when it is measured honestly. The
//! definition census beside the rate is the other half of tier 2's
//! deliverable and is asserted exactly, by kind, by block type and by name
//! with declaration lines, because no rate over 24 references could ever see
//! a definition bug.
//!
//! # The two numbers a reader of the baseline should expect
//!
//! - **`local_binding` is zero, and stays zero.** Tier 2 emits no
//!   expression-level reference, so nothing here *can* name a local; a
//!   non-zero count would mean the contract above had been widened, and the
//!   baseline fails on drift in it.
//! - **`external` is one, and it is a measured fact.** Exactly one `module`
//!   block in the corpus names a public registry address, which is a package
//!   `terraform init` fetches and never a directory on disk. [`resolve`] says
//!   at length why "not a local path" is *not* how that judgement is made —
//!   `External` sits outside both terms of the rate, and a rule that widens
//!   it raises the rate without linking anything.
//!
//! A baseline is recorded with `arthron gate --rebase`. HCL's rate is HCL's
//! own and is never averaged into anyone else's — and it is an import rate,
//! which is not the measurement a tier-1 language's rate is.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod resolve;

/// HCL's registration. **Live**: the track owns `.tf`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for it and the
/// driver runs [`resolve::scan_hcl`] over every HCL file the walk reaches.
pub const TRACK: Track = Track {
    name: "hcl",
    langs: &[Lang::Hcl],
    scan: Some(resolve::scan_hcl_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hcl_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Hcl]);
        assert!(Lang::Hcl.owns_extension("tf"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("tf"));
        // HCL reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::Hcl.domain(), crate::model::Domain::Hcl);
    }

    #[test]
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed `.tf` and deliberately left
        // `.tfvars`, `.hcl` and `.nomad` unclaimed; the honest moment to
        // widen that list is a commit that measures the files it adds, and
        // this one does not. The pinned grammar reads all of them — the
        // claim this track makes is about `.tf`.
        assert_eq!(Lang::Hcl.extensions(), ["tf"]);
        for unclaimed in ["tfvars", "hcl", "nomad", "tofu"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
    }
}
