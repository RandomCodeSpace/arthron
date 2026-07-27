//! The PHP track. **Live.** Owns `.php`. **Tier 2.**
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_php`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `php` and the
//! driver runs PHP over every `.php` file the walk reaches. Going live edited
//! this file and this track's own modules, which is the whole of the
//! registry's zero-conflict rule — see [`crate::registry`] for why, and
//! [`crate::track_go`] for the shape a live track takes.
//!
//! # Tier 2 is a smaller promise, kept exactly
//!
//! Tier 1 is definitions, references and a call graph. **Tier 2 is
//! definitions, structure and imports, and its gate is an import-resolution
//! rate.** So this track emits one reference kind — [`crate::model::RefKind::Import`],
//! the `use` statement — and no call site, no type use and no supertype. That
//! is not a staging decision to be quietly relaxed: a tier-2 language that
//! emitted call references un-gated would put them in a denominator no tier-2
//! resolver links, which is tier-1 coverage claimed without tier-1 work.
//!
//! What tier 2 keeps in full is the never-drop contract. Every `use` this
//! track extracts ends `Resolved`, `External`, or `Unresolved` with a reason,
//! and the reasons come from the ratified taxonomy — none was added for PHP.
//!
//! Three layers, and the boundary between them is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**. It is also
//!   where PHP's one genuinely hostile piece of syntax is settled: `use`
//!   means three unrelated things depending on where it sits.
//! - [`project`] — phase 0: the PSR-4 map `composer.json` declares. PHP
//!   states a class's namespace in the source and its *location* in a
//!   manifest, so this is what one `go.mod` line is to Go.
//! - [`resolve`] — the one place a PHP [`crate::Outcome`] is produced.
//!
//! # The honesty posture, and what it costs
//!
//! PHP's floor on the vendored corpus is a *sibling package under this
//! repository's own vendor namespace root*. `guzzlehttp/psr7` supplies
//! `GuzzleHttp\Psr7\…`, and guzzle's own manifest maps `GuzzleHttp\` to
//! `src/` — so PSR-4 says those names belong at `src/Psr7/…` and nothing is
//! there. They are [`crate::UnresolvedReason::ModuleNotFound`] and they count
//! against the rate.
//!
//! Calling them `External` instead would take them out of *both* terms and
//! lift the rate to a perfect 1.0 without linking one extra reference. The
//! facts that would close the gap honestly are a `composer.lock` or an
//! installed `vendor/` tree; neither is in a corpus, and a package name does
//! not give its namespace — `guzzlehttp/promises` supplies
//! `GuzzleHttp\Promise`, which is neither segment studly-cased — so there is
//! no derivation to write, only a guess to decline.
//!
//! # Known under-counts
//!
//! Recorded here rather than left to be rediscovered, because each is a
//! *known* shortfall and none may be closed by widening a bucket:
//!
//! - **`use function` and `use const` are unexercised.** The vendored corpus
//!   contains neither, and no `files` autoload entry, so PHP's
//!   fallback-to-global rule for an unqualified function call is not measured
//!   here. The rules exist and the fixtures check them; the corpus does not.
//!   That shape needs a second PHP corpus.
//! - **Nested `composer.json` files are not read.** A monorepo whose packages
//!   each carry one declares prefixes this build does not see.
//! - **`psr-0`, `classmap` and `files` autoload entries are not read**, and
//!   neither is `define()`, `class_alias()` or anything else that declares a
//!   name by executing code. arthron does not execute code.
//! - **Names are case-insensitive in PHP; identities here are not.** A
//!   project that autoloads at all already spells its names consistently,
//!   because PSR-4 maps a name onto a path on a case-sensitive filesystem.
//!
//! Sharing the store with the other live tracks is safe in both directions: a
//! scan forgets only files carrying an extension the running track owns, and
//! extension ownership is a partition (see [`crate::model::Lang::for_extension`]);
//! the manifest fence is per language. PHP takes [`crate::model::Domain::Php`]
//! alone — no other language's resolver has to find a PHP declaration, and a
//! shared identity space is a capability claim rather than a convenience.
//!
//! A baseline is recorded with `arthron gate --rebase`. PHP's rate is PHP's
//! own and is never averaged into anyone else's — and it is an import rate,
//! which is not the same measurement a tier-1 language's rate is.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod project;
pub mod resolve;

/// PHP's registration. **Live**: the track owns `.php`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for it and the
/// driver runs [`resolve::scan_php`] over every PHP file the walk reaches.
pub const TRACK: Track = Track {
    name: "php",
    langs: &[Lang::Php],
    scan: Some(resolve::scan_php_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Php]);
        assert!(Lang::Php.owns_extension("php"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("php"));
        // The extension list is exactly what registration committed: going
        // live claims no new spelling.
        assert_eq!(Lang::Php.extensions(), ["php"]);
        // PHP reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::Php.domain(), crate::model::Domain::Php);
    }
}
