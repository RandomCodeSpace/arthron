//! Rust's [`Language`] impl: the constants the track is reported under and the
//! three types only Rust's own layers may read.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_rust::extract::RsHeader;
use crate::track_rust::project::RsWorkspace;
use crate::track_rust::resolve::RsScope;

/// The Rust language. Stateless; only its associated types carry anything.
pub struct RsLang;

impl Language for RsLang {
    const LANG: Lang = Lang::Rust;
    const DOMAIN: Domain = Domain::Rust;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Rust owns and this one cannot drift apart.
    fn extensions() -> &'static [&'static str] {
        Lang::Rust.extensions()
    }

    /// `target/` holds build output, including the expanded sources of every
    /// dependency a build script or a proc macro generated. Descending into
    /// one would index a dependency as if the repository had written it,
    /// inventing in-repository definitions that inflate the resolution rate
    /// with links to code the repository does not own.
    fn skip_dirs() -> &'static [&'static str] {
        &["target"]
    }

    type Header = RsHeader;
    type Scope = RsScope;
    type Config = RsWorkspace;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_reports_as_rust_and_hashes_in_the_rust_domain() {
        assert_eq!(RsLang::LANG, Lang::Rust);
        assert_eq!(RsLang::DOMAIN, Domain::Rust);
        assert_eq!(RsLang::LANG.domain(), RsLang::DOMAIN);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(RsLang::extensions(), Lang::Rust.extensions());
        assert_eq!(RsLang::extensions(), ["rs"]);
    }

    #[test]
    fn build_output_is_never_descended_into() {
        assert!(RsLang::skip_dirs().contains(&"target"));
    }
}
