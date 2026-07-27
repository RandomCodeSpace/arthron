//! Python's [`Language`] impl: the constants the track is reported under and
//! the three types only Python's own layers may read.
//!
//! All three are now filled. [`PyHeader`] is what one file states about
//! itself, [`PyScope`] is what the resolver reads that file's references
//! against, and [`PyProject`] is the layout — which directories are packages,
//! which are import roots, what the project declares as a dependency. The
//! core moves all three and inspects none of them.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_python::extract::PyHeader;
use crate::track_python::project::PyProject;
use crate::track_python::resolve::PyScope;

/// The Python language. Stateless; only its associated types carry anything.
pub struct PyLang;

impl Language for PyLang {
    const LANG: Lang = Lang::Python;
    const DOMAIN: Domain = Domain::Python;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Python owns and this one cannot drift apart.
    fn extensions() -> &'static [&'static str] {
        Lang::Python.extensions()
    }

    /// Virtual environments and caches hold copies of third-party sources.
    /// Descending into one would index a dependency as if the repository had
    /// written it, inventing in-repository definitions that inflate the
    /// resolution rate with links to code the repository does not own.
    fn skip_dirs() -> &'static [&'static str] {
        &[".venv", "venv", "__pycache__", ".tox"]
    }

    type Header = PyHeader;
    type Scope = PyScope;
    type Config = PyProject;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_reports_as_python_and_hashes_in_the_python_domain() {
        assert_eq!(PyLang::LANG, Lang::Python);
        assert_eq!(PyLang::DOMAIN, Domain::Python);
        assert_eq!(PyLang::LANG.domain(), PyLang::DOMAIN);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(PyLang::extensions(), Lang::Python.extensions());
        assert_eq!(PyLang::extensions(), ["py"]);
        // A `.pyi` stub is not a `.py` file, so A-09's "skip the stub when a
        // sibling `.py` exists" needs no rule: the walk never offers one.
        assert!(!PyLang::extensions().contains(&"pyi"));
    }

    #[test]
    fn virtual_environments_are_never_descended_into() {
        for dir in [".venv", "venv", "__pycache__", ".tox"] {
            assert!(PyLang::skip_dirs().contains(&dir));
        }
    }
}
