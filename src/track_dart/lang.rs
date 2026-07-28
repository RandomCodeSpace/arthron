//! Dart's [`Language`] impl: the constants the track is reported under, the
//! three types only Dart's own layers may read, and the FQN grammar every one
//! of them agrees on.
//!
//! # The FQN grammar
//!
//! ```text
//! '$' <repo-relative path>  ( '::' <owner> '.' )? <name>
//! ```
//!
//! Dart has no namespace above the file. A **library** is a file, and every
//! declaration it writes belongs to it and to nothing wider: `Equality` in
//! `lib/src/equality.dart` and `Equality` in some other package's file are two
//! entities that no dotted name distinguishes, because Dart never spells one.
//! So the library's own path is the root of every identity beneath it, and the
//! path is also exactly what an `import` names — which is why the two are one
//! FQN space rather than two.
//!
//! Three marks, each doing one job:
//!
//! - **`$` opens a library path.** Every Dart FQN starts with it, so
//!   [`crate::pipeline`]'s `external:` prefix is unreachable from this domain
//!   whatever a repository names its top-level directories.
//! - **`::` crosses from the library into its declarations.** It is not a
//!   token Dart's grammar admits anywhere in a declaration, so the only way to
//!   confuse a library FQN with a declaration FQN is a *directory* whose name
//!   both begins with `$` and contains `::` — the residual this grammar
//!   accepts, stated rather than left to be discovered.
//! - **`.` joins an owner to its member,** which is how Dart itself writes
//!   `DelegatingList.add` and `QueueList.from`.
//!
//! # One member namespace per type, which is Dart's own rule
//!
//! No `#`/`.` split like Scala's or Ruby's: Dart forbids a static and an
//! instance member of one name in one type (§ "It is a compile-time error if a
//! class has an instance member and a static member with the same name"), so
//! `C.foo` names at most one declaration and a second separator would buy
//! nothing. A constructor is spelled with Dart's own tear-off name — `C.new`
//! for the unnamed one, `C.named` for a named one — and cannot collide with a
//! method, because `new` is a reserved word and no member may be called it.
//!
//! # `_` is the visibility, and it is a library's, not a type's
//!
//! A Dart name beginning with `_` is private **to its library**, so
//! [`crate::model::DefFacets::EXPORTED`] is set for every other name and
//! cleared for these. [`crate::model::DefFacets::PRIVATE`] is deliberately
//! *not* set: that bit means "not inherited by anything below it", and a
//! Dart private member is inherited perfectly well by a subclass in the same
//! library. Setting it would shorten a supertype closure this track does not
//! even build, and would be wrong the day one is.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_dart::extract::DartHeader;
use crate::track_dart::project::DartProject;
use crate::track_dart::resolve::DartScope;

/// The Dart language. Stateless; only its associated types carry anything.
pub struct DartLang;

impl Language for DartLang {
    const LANG: Lang = Lang::Dart;
    const DOMAIN: Domain = Domain::Dart;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Dart owns and this one cannot drift apart.
    fn extensions() -> &'static [&'static str] {
        Lang::Dart.extensions()
    }

    /// Where pub and the SDK write, and where neither this repository's
    /// author nor its reader looks.
    ///
    /// `.dart_tool` holds `package_config.json` and every generated library a
    /// build step produced; `build` is `build_runner`'s output root. Both hold
    /// real `.dart` files, and indexing either would mint in-repository
    /// definitions the repository did not write — which inflates a resolution
    /// rate by giving misses somewhere to land.
    fn skip_dirs() -> &'static [&'static str] {
        &[".dart_tool", "build"]
    }

    type Header = DartHeader;
    type Scope = DartScope;
    type Config = DartProject;
}

/// The reserved character every Dart identity opens with, and nothing else
/// may.
pub const LIBRARY: char = '$';

/// The mark between a library and a declaration inside it.
pub const MEMBER: &str = "::";

/// The library FQN of a repo-relative path: `lib/src/wrappers.dart` →
/// `$lib/src/wrappers.dart`.
///
/// Total, because every `.dart` file the walk reaches is a library an
/// `import` may name whether or not it declares anything, and the suffix is
/// kept because a Dart URI always carries it — `import 'utils'` is not Dart.
pub fn library_fqn(rel_path: &str) -> String {
    format!("{LIBRARY}{rel_path}")
}

/// The FQN of a declaration in one library: `owner` outermost first, then the
/// declared name.
pub fn decl_fqn(rel_path: &str, owner: &[String], name: &str) -> String {
    let lib = library_fqn(rel_path);
    if owner.is_empty() {
        format!("{lib}{MEMBER}{name}")
    } else {
        format!("{lib}{MEMBER}{}.{name}", owner.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dart_reports_as_dart_and_hashes_in_the_dart_domain() {
        assert_eq!(DartLang::LANG, Lang::Dart);
        assert_eq!(DartLang::DOMAIN, Domain::Dart);
        assert_eq!(DartLang::LANG.domain(), DartLang::DOMAIN);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(DartLang::extensions(), Lang::Dart.extensions());
        assert_eq!(DartLang::extensions(), ["dart"]);
    }

    #[test]
    fn a_library_identity_cannot_be_spelled_by_a_declaration() {
        assert_eq!(
            library_fqn("lib/src/wrappers.dart"),
            "$lib/src/wrappers.dart"
        );
        assert_eq!(
            decl_fqn("lib/src/wrappers.dart", &[], "DelegatingList"),
            "$lib/src/wrappers.dart::DelegatingList",
        );
        assert_eq!(
            decl_fqn(
                "lib/src/wrappers.dart",
                &["DelegatingList".to_string()],
                "add",
            ),
            "$lib/src/wrappers.dart::DelegatingList.add",
        );
        // A declaration FQN is its library's FQN plus the mark: containment
        // is spelled, not implied.
        assert!(decl_fqn("lib/a.dart", &[], "X").starts_with(&library_fqn("lib/a.dart")),);
        // Every identity opens with the reserved character, so `external:` is
        // unreachable from this domain.
        assert!(library_fqn("lib/a.dart").starts_with(LIBRARY));
        assert!(decl_fqn("lib/a.dart", &[], "X").starts_with(LIBRARY));
    }

    #[test]
    fn two_libraries_declaring_one_name_are_two_identities() {
        // The whole reason the path roots the grammar: Dart has no namespace
        // above the file, so a flat dotted name would merge these two.
        assert_ne!(
            decl_fqn("lib/src/equality.dart", &[], "Equality"),
            decl_fqn("lib/equality.dart", &[], "Equality"),
        );
    }
}
