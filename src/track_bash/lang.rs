//! Bash's [`Language`] impl: the constants the track is reported under, the
//! three types only Bash's own layers may read, and the FQN grammar every one
//! of them agrees on.
//!
//! # The FQN grammar
//!
//! ```text
//! '$' <repo-relative path> ( '#' <function> ( '.' <function> )* )?
//! ```
//!
//! Two kinds of thing, and one identity space that holds both:
//!
//! - A **script** — what `source` names. Written `$` + the repo-relative path
//!   **with its extension**: `$lib/bats-core/common.bash`, `$install.sh`. The
//!   extension stays because `source` spells it: unlike Ruby's `require`,
//!   bash's `source` names a file and not a feature, and stripping a suffix
//!   the reference wrote would make two different files one node.
//! - A **function**, qualified by the file that writes it:
//!   `$lib/bats-core/common.bash#bats_trim`. A function written inside
//!   another joins the chain with a `.`:
//!   `$lib/bats-core/test_functions.bash#outer.inner` reads as the
//!   file, then `outer`, then `inner`.
//!
//! # Why a function is qualified by its file
//!
//! Bash's function table is flat and per **shell process**: once
//! `lib/bats-core/common.bash` is sourced, everything it declared is callable
//! from everywhere else in that shell. A scan measures a *tree*, not a
//! process. Two scripts in a repository that each write `usage()` are two
//! declarations of two different things that happen to share a name, and
//! folding them into one node would make the definition census — the whole of
//! what this track delivers beyond its import rate — under-report by exactly
//! the number of collisions. Nothing here resolves a function *name*, so
//! qualifying costs no edge; it only keeps the count honest.
//!
//! # The two reserved marks
//!
//! `$` opens every identity in this domain, which is what keeps
//! [`crate::pipeline`]'s `external:` prefix unreachable from here: an
//! `external:` key begins with a letter and every key this track mints begins
//! with `$`.
//!
//! `#` separates the file from the chain, and `.` joins chained names.
//! Neither is reserved by the language: `#` opens a comment only at the start
//! of a word, so `a#b` is a legal function name, and `.` is ordinary. The
//! separation survives on a weaker property — every path the walk offers ends
//! in an extension this track owns — so the only way two identities can
//! collide is a *file* whose own name contains a `#` followed by a second
//! owned extension, beside a function named for the remainder. Nothing in the
//! measured corpus writes one, and the alternative — a mark no name may carry
//! — would be a byte no report could print.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_bash::extract::BashHeader;
use crate::track_bash::resolve::BashScope;

/// The Bash language. Stateless; only its associated types carry anything.
pub struct BashLang;

/// Phase 0 for Bash: deliberately empty.
///
/// Every track here that reads a manifest reads one because the language
/// states a name its source does not — Go's module path, Rust's crate roots,
/// Ruby's load path, PHP's PSR-4 prefixes. **Bash states nothing anywhere.**
/// There is no manifest, no package manager and no module map; what a
/// `source` resolves against is the process's working directory and `$PATH`,
/// both of which are environment rather than repository facts.
///
/// The measured corpus carries a `package.json`, and it is deliberately not
/// read: its `files` array decides which scripts npm *ships*, which is a
/// packaging fact and not a resolution one. Reading it would put a name into
/// every identity in the graph on the strength of an analogy.
///
/// So the digest is empty and a Bash scan is never invalidated by a manifest
/// — the contract [`crate::lang::Resolver::config_digest`] already states for
/// a language with no project manifest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BashProject;

impl Language for BashLang {
    const LANG: Lang = Lang::Bash;
    const DOMAIN: Domain = Domain::Shell;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Bash owns and this one cannot drift apart.
    ///
    /// `.bats` is **not** claimed, and neither is an extensionless script.
    /// See [`crate::track_bash`] for both measurements.
    fn extensions() -> &'static [&'static str] {
        Lang::Bash.extensions()
    }

    /// None, and that is a measurement rather than an omission.
    ///
    /// Every other track names the directory its own language's tooling
    /// writes or unpacks into — `vendor`, `target`, `node_modules`, `.venv`.
    /// Bash has no package manager, no build output and no vendoring
    /// convention, so there is no directory this track can call somebody
    /// else's code without guessing. A repository that vendors shell scripts
    /// says so in its own `arthron.toml` include/exclude, which is where a
    /// repository's decision about its own layout belongs.
    fn skip_dirs() -> &'static [&'static str] {
        &[]
    }

    type Header = BashHeader;
    type Scope = BashScope;
    type Config = BashProject;
}

/// The reserved prefix every identity in this domain carries.
pub const SCRIPT: char = '$';

/// The mark separating a file from the function chain inside it.
pub const MEMBER: char = '#';

/// The script FQN of a repo-relative path: `lib/util.bash` →
/// `$lib/util.bash`.
///
/// Total, because every owned file the walk reaches is a script whether or
/// not it declares a function: a `source` naming an empty file still
/// resolves.
pub fn script_fqn(rel_path: &str) -> String {
    format!("{SCRIPT}{rel_path}")
}

/// The FQN of a function `rel_path` declares under `owner`.
pub fn function_fqn(rel_path: &str, owner: &[String], name: &str) -> String {
    let mut out = script_fqn(rel_path);
    out.push(MEMBER);
    for segment in owner {
        out.push_str(segment);
        out.push('.');
    }
    out.push_str(name);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_reports_as_bash_and_hashes_in_the_shell_domain() {
        assert_eq!(BashLang::LANG, Lang::Bash);
        assert_eq!(BashLang::DOMAIN, Domain::Shell);
        assert_eq!(BashLang::LANG.domain(), BashLang::DOMAIN);
        assert_eq!(BashLang::LANG.tier(), 2);
        assert_eq!(BashLang::LANG.rate_scope(), "import resolution");
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(BashLang::extensions(), Lang::Bash.extensions());
        assert_eq!(BashLang::extensions(), ["sh", "bash"]);
        // Claimed by nobody, each for a measured reason — see the track's
        // module docs. Going live widens nothing the registration had not.
        for unclaimed in ["bats", "zsh", "ksh", "sh.in"] {
            assert!(!BashLang::extensions().contains(&unclaimed));
        }
    }

    #[test]
    fn a_script_keeps_the_extension_a_source_writes() {
        assert_eq!(script_fqn("lib/util.bash"), "$lib/util.bash");
        assert_eq!(script_fqn("install.sh"), "$install.sh");
        // Two files whose stems agree are two nodes: `source` names the file,
        // so stripping the suffix would merge them.
        assert_ne!(script_fqn("a/util.sh"), script_fqn("a/util.bash"));
    }

    #[test]
    fn a_function_is_qualified_by_the_file_that_writes_it() {
        assert_eq!(
            function_fqn("lib/util.bash", &[], "hi"),
            "$lib/util.bash#hi",
        );
        assert_eq!(
            function_fqn("lib/util.bash", &["outer".to_string()], "inner"),
            "$lib/util.bash#outer.inner",
        );
        // The whole reason the file is in the name.
        assert_ne!(
            function_fqn("bin/a.sh", &[], "usage"),
            function_fqn("bin/b.sh", &[], "usage"),
        );
        // A script and a function of the same file are two identities.
        assert_ne!(
            script_fqn("lib/util.bash"),
            function_fqn("lib/util.bash", &[], "hi"),
        );
    }

    #[test]
    fn every_identity_starts_at_the_reserved_prefix() {
        // Which is what keeps the driver's `external:` prefix unreachable
        // from this domain — and `$` is the one character a bash word cannot
        // carry literally, so no source text can spell it.
        for fqn in [
            script_fqn("lib/util.bash"),
            function_fqn("lib/util.bash", &[], "hi"),
        ] {
            assert!(fqn.starts_with(SCRIPT), "{fqn}");
            assert!(!fqn.starts_with("external:"));
        }
    }
}
