//! The Bash track. **Live.** Owns `.sh` and `.bash`, at **tier 2,
//! best-effort**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_bash_with`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for both
//! extensions and the driver runs Bash over every file the walk reaches.
//! Three layers, and the boundary between them is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`resolve`] — the one place a Bash [`crate::Outcome`] is produced. Every
//!   reference ends `Resolved`, `External`, or `Unresolved(reason)`, and
//!   there is no way to express "dropped".
//! - [`lang`] — the [`crate::lang::Language`] impl, the FQN grammar the other
//!   two agree on, and [`lang::BashProject`], which is empty on purpose:
//!   there is no manifest in this language to read.
//!
//! # What best-effort tier 2 means here, precisely
//!
//! Definitions, structure, and import-like references. **No call edges**, and
//! the honest consequence is sharper in shell than in any other track: a bash
//! call site *is* a `command` node, spelled exactly like `printf` or `ls`, so
//! an extractor that emitted calls would turn every command in the tree into
//! a reference and the resolution rate would become a statement about how
//! much of coreutils lives in the repository. So the reference kinds this
//! track emits are `source` and `.` — the two spellings of one builtin — and
//! nothing else.
//!
//! The definitions beside them are the deliverable: every function the tree
//! declares, qualified by the file that writes it, plus the *script* every
//! owned file is.
//!
//! # The number a reader of the baseline should expect, and why
//!
//! **The rate is 0.0%, over a denominator of 6.** That is the measurement,
//! not a placeholder. The corpus was chosen because not one of its `source`
//! targets is a literal path: the six clauses in the files this track owns
//! are three of the shape `source "$BATS_ROOT/$BATS_LIBDIR/bats-core/<n>.bash"`
//! — both variables computed at run time, one from the resolved path of `$0`
//! — and three that are pure runtime values (`"${BATS_TEST_SOURCE?}"`,
//! `"$library_load_path"`, `"$1"`). Every one is
//! [`crate::UnresolvedReason::DynamicModuleSpecifier`], with **nothing
//! probed**.
//!
//! The tail of each composed path really does name a file in this tree, and
//! matching on it would take the rate from 0% to 50% in one commit. It would
//! also be a guess about two variables the running program computes, and the
//! first repository whose `BATS_LIBDIR` is not `lib` would get confidently
//! wrong edges. A rate of zero stated honestly is the deliverable here; the
//! definition census beside it is what the track is for.
//!
//! # The two buckets that sit outside the rate
//!
//! - **`local_binding` is zero, and stays zero.** It is the one bucket the
//!   rate's own definition lets a resolver move references into without
//!   linking anything. Tier 2 emits no expression-level reference, so nothing
//!   here *can* name a local; a non-zero count would mean the contract above
//!   had been widened, and the baseline fails on drift in it.
//! - **`external` is zero, and stays zero.** Bash has no manifest, so no
//!   repository declares that a name comes from outside it. Every path that
//!   leaves the tree — an absolute one, one that climbs above the root, a
//!   bare name that would come off `$PATH` — is `UnknownPackage` and counts
//!   *against* the rate. A track that mints no `External` cannot raise its
//!   rate by reclassifying.
//!
//! # What this track does not own, and why each is measured rather than
//! assumed
//!
//! - **`.bats` is not claimed.** The shell grammar does not reject a
//!   `@test "name" { … }` block, it *misreads* one: the header and the
//!   closing brace each come back as an ordinary `command` where a real shell
//!   function comes back as one `function_definition`. Reading them would
//!   yield records for things the file does not declare, which is worse than
//!   not reading it — see `docs/decisions.md`, 2026-07-27. Twenty-one of the
//!   corpus's forty-five shell files are `.bats`.
//! - **An extensionless script is not claimed.** Twelve of the corpus's files
//!   — `bin/bats` and the eleven `libexec/bats-core/*` — are named after the
//!   commands they are, and only their shebang says what language they hold.
//!   Ownership here is by extension, and a shebang walk is a scan-wide
//!   capability rather than a bash decision.
//! - **`load` never appears.** It is not shell syntax: bats defines it as a
//!   function in `lib/bats-core/test_functions.bash`, and its twenty-nine
//!   call sites are all in `.bats` files this track does not read. So it
//!   contributes no reference, no miss and no reason — recorded here so its
//!   absence from the tally is a fact rather than a gap.
//!
//! **The corpus surface this track actually scans is 12 files** — the two
//! `.sh` installers and the ten `.bash` libraries — out of the 45 shell files
//! in the snapshot. Every number in `baselines/bash-bats-core.toml` is a fact
//! about those twelve.
//!
//! A baseline is recorded with `arthron gate --rebase`. Bash's rate is Bash's
//! own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod resolve;

/// Bash's registration. **Live**: the track owns `.sh` and `.bash`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for both and the
/// driver runs [`resolve::scan_bash`] over every shell file the walk reaches.
pub const TRACK: Track = Track {
    name: "bash",
    langs: &[Lang::Bash],
    scan: Some(resolve::scan_bash_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Bash]);
        assert!(Lang::Bash.owns_extension("sh"));
        assert!(Lang::Bash.owns_extension("bash"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("sh"));
        assert!(TRACK.owns_extension("bash"));
        // Bash reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::Bash.domain(), crate::model::Domain::Shell);
    }

    #[test]
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed `.sh` and `.bash` and
        // deliberately left `.bats` unclaimed after measuring what the shell
        // grammar does to one. Going live measures the files it reads; it
        // does not widen the claim.
        assert_eq!(Lang::Bash.extensions(), ["sh", "bash"]);
        for unclaimed in ["bats", "zsh", "ksh", "fish"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
        // And an extensionless script is owned by nobody: `Path::extension`
        // answers `None` for `bin/bats`, so the walk never offers it.
        assert_eq!(Lang::for_extension(""), None);
    }
}
