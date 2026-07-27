//! PHP's [`Language`] impl: the constants the track is reported under and the
//! three types only PHP's own layers may read.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_php::extract::PhpHeader;
use crate::track_php::project::PhpProject;
use crate::track_php::resolve::PhpScope;

/// The PHP language. Stateless; only its associated types carry anything.
pub struct PhpLang;

impl Language for PhpLang {
    const LANG: Lang = Lang::Php;
    const DOMAIN: Domain = Domain::Php;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what PHP owns and this one cannot drift apart.
    fn extensions() -> &'static [&'static str] {
        Lang::Php.extensions()
    }

    /// `vendor/` holds the sources composer installed. Descending into one
    /// would index a dependency as if this repository had written it, which
    /// invents in-repository definitions and inflates the resolution rate
    /// with links to code the repository does not own — the same hazard
    /// Python's `.venv` carries.
    fn skip_dirs() -> &'static [&'static str] {
        &["vendor"]
    }

    type Header = PhpHeader;
    type Scope = PhpScope;
    type Config = PhpProject;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_reports_as_php_and_hashes_in_the_php_domain() {
        assert_eq!(PhpLang::LANG, Lang::Php);
        assert_eq!(PhpLang::DOMAIN, Domain::Php);
        assert_eq!(PhpLang::LANG.domain(), PhpLang::DOMAIN);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(PhpLang::extensions(), Lang::Php.extensions());
        assert_eq!(PhpLang::extensions(), ["php"]);
        // `.phtml`, `.php4` and the rest stay unclaimed: the go-live commit
        // is the first honest moment to widen the list, and nothing here has
        // parsed one.
        for ext in ["phtml", "php4", "php5", "inc"] {
            assert!(!PhpLang::extensions().contains(&ext));
        }
    }

    #[test]
    fn composers_vendor_tree_is_never_descended_into() {
        assert!(PhpLang::skip_dirs().contains(&"vendor"));
    }
}
