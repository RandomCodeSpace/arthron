//! C++'s [`Language`] impl: the constants the track is reported under, the
//! three types only C++'s own layers may read, and the FQN grammar every one
//! of them agrees on.
//!
//! # The FQN grammar
//!
//! C++ names three different kinds of thing here, and one identity space has
//! to hold all three without any of them being able to spell another:
//!
//! - A **unit** — what `#include` names. Written [`UNIT`] + the
//!   repository-relative path: `#src/format.cc`, `#test/util.h`. `#` is the
//!   character that *opens a preprocessing directive*, so it can never appear
//!   in a C++ qualified-id ([lex.name]), and `#include` is exactly the
//!   directive that names one of these.
//! - A **named module** — what a C++20 `import` names. Written [`MODULE`] +
//!   the module name: `@fmt`, `@std`. `@` is not a token of C++ at all, which
//!   matters because a module and a namespace routinely share a name: fmt
//!   writes both `export module fmt;` and `namespace fmt`, and they are two
//!   entities.
//! - An **entity** and its members, written the way C++ writes them:
//!   `fmt::detail`, `fmt::detail::buffer`, `fmt::detail::buffer::append`.
//!
//! Neither reserved character can begin an entity FQN, so the three spaces
//! are disjoint by the language's own grammar rather than by convention. Both
//! also keep [`crate::pipeline`]'s `external:` prefix unreachable from this
//! domain.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_cpp::extract::CppHeader;
use crate::track_cpp::project::CppProject;
use crate::track_cpp::resolve::CppScope;

/// The C++ language. Stateless; only its associated types carry anything.
pub struct CppLang;

impl Language for CppLang {
    const LANG: Lang = Lang::Cpp;
    const DOMAIN: Domain = Domain::Cxx;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what C++ owns and this one cannot drift apart.
    ///
    /// `.h` **is** claimed — the amendment the tier-2 registration reserved,
    /// made because a header-dominated C++ library is unmeasurable without
    /// it; see [`crate::model::Lang::extensions`] for the measurement and
    /// the accepted risk. `.c` is **not**: a C translation unit read under
    /// the C++ grammar is the wrong language, and that claim waits for a C
    /// track.
    fn extensions() -> &'static [&'static str] {
        Lang::Cpp.extensions()
    }

    /// Directories holding somebody else's source. Descending into one would
    /// index a dependency as if this repository had written it, inventing
    /// in-repository definitions that inflate the resolution rate.
    ///
    /// `build`, `_build` and `cmake-build-debug` are the conventional CMake
    /// output trees; a generated source read from one is a definition no file
    /// in this repository declares.
    fn skip_dirs() -> &'static [&'static str] {
        &[
            "third_party",
            "thirdparty",
            "build",
            "_build",
            "cmake-build-debug",
            "cmake-build-release",
        ]
    }

    type Header = CppHeader;
    type Scope = CppScope;
    type Config = CppProject;
}

/// The reserved prefix a translation- or header-unit identity carries, and
/// nothing else may.
pub const UNIT: char = '#';

/// The reserved prefix a C++20 named-module identity carries, and nothing
/// else may.
pub const MODULE: char = '@';

/// The unit FQN of a repository-relative path: `src/format.cc` →
/// `#src/format.cc`.
///
/// The extension is kept, unlike Ruby's feature names: `#include` spells the
/// file in full, `os.h` and `os.cc` are two units, and dropping the suffix
/// would merge them.
pub fn unit_fqn(rel_path: &str) -> String {
    format!("{UNIT}{rel_path}")
}

/// The FQN of a C++20 named module: `fmt` → `@fmt`.
pub fn module_fqn(name: &str) -> String {
    format!("{MODULE}{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpp_reports_as_cpp_and_hashes_in_the_cxx_domain() {
        assert_eq!(CppLang::LANG, Lang::Cpp);
        assert_eq!(CppLang::DOMAIN, Domain::Cxx);
        assert_eq!(CppLang::LANG.domain(), CppLang::DOMAIN);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own_and_h_is_its_ratified_widening() {
        assert_eq!(CppLang::extensions(), Lang::Cpp.extensions());
        assert_eq!(
            CppLang::extensions(),
            ["cpp", "cc", "cxx", "h", "hpp", "hh", "hxx"],
        );
        // `.c` stays unclaimed. The first honest moment to claim an
        // extension is the commit that parses it: the `.h` claim was made
        // by exactly such a commit, and no commit parses C as C.
        assert!(!CppLang::extensions().contains(&"c"));
    }

    #[test]
    fn a_unit_a_module_and_an_entity_cannot_spell_each_other() {
        assert_eq!(unit_fqn("src/format.cc"), "#src/format.cc");
        assert_eq!(unit_fqn("include/fmt/os.h"), "#include/fmt/os.h");
        assert_eq!(module_fqn("fmt"), "@fmt");
        // fmt declares `export module fmt;` and `namespace fmt` both. They
        // are two entities and must be two identities.
        assert_ne!(module_fqn("fmt"), "fmt");
        // Neither reserved character may open a C++ qualified-id.
        assert!(unit_fqn("a").starts_with(UNIT));
        assert!(module_fqn("a").starts_with(MODULE));
        assert_ne!(UNIT, MODULE);
    }
}
