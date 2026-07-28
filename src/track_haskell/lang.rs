//! Haskell's [`Language`] impl: the constants the track is reported under, the
//! three types only Haskell's own layers may read, and the FQN grammar every
//! one of them agrees on.
//!
//! # The FQN grammar
//!
//! ```text
//! <module>  ::= <repo-relative path with ".hs" stripped>
//! <decl>    ::= <module> '#' <name> ( '.' <name> )*
//! ```
//!
//! **A module's identity is its file, not its declared name.** That is the one
//! decision this grammar turns on, and the measured corpus is what forces it:
//! six files in aeson declare `module Main` — three executables under
//! `examples/src/`, the aeson test driver, and text-iso8601's test and
//! benchmark drivers. They are six different modules that GHC compiles into
//! six different programs, and nothing in any `.hs` file tells them apart.
//! Only a component's `hs-source-dirs` plus its `main-is` does, and only the
//! file path carries both. Naming them by their declared name would merge six
//! entities into one node; naming them by their path keeps six, and keeps the
//! grammar injective without a single special case.
//!
//! The declared name is not thrown away — it is what an `import` spells, and
//! [`crate::track_haskell::resolve`] turns one into a path with the source
//! roots the `.cabal` manifests declare. That is GHC's own lookup: a home
//! module `A.B.C` is the file `<root>/A/B/C.hs` for some `hs-source-dirs`
//! root of the component being built.
//!
//! # Two namespaces, kept apart by nesting
//!
//! Haskell's type and value namespaces are disjoint, and `newtype Key = Key
//! { unKey :: Text }` uses both for one word: a type constructor `Key` and a
//! data constructor `Key`. A flat `<module>#<name>` would hash them to one
//! identity and lose whichever was written second. So a data constructor, a
//! record field and a class method are each filed **under the declaration
//! they belong to** — `…#Key.Key`, `…#Key.unKey`, `…#ToJSON.toJSON` — while
//! the type, the class and every top-level binding sit directly under the
//! module. Within one legal Haskell module that is injective: two types
//! cannot share a name, two constructors cannot, and a type and a top-level
//! function cannot collide because one is capitalised and the other is not.
//!
//! # The two reserved characters, and what Haskell does to them
//!
//! The house grammar reserves `#` to separate a container from its members
//! and `:` to keep [`crate::pipeline`]'s `external:` prefix unreachable.
//! Haskell is the first language here that can write **both** inside a name:
//! `#` and `:` are operator characters, so `(#)` and `(.:)` are ordinary
//! declarations, and `(.:)` is one aeson exports.
//!
//! Neither costs anything, because of where they may appear:
//!
//! - **`#`** is split on **first occurrence**, and the part before it is a
//!   file path. A definition FQN therefore always has a `#`, a module FQN has
//!   one only if a path does, and no module a Haskell `import` can name may
//!   contain one — a module name is dot-separated `module_id`s, which are
//!   letters, digits, `_` and `'`. So the resolver can never *generate* a
//!   candidate that reads as a definition.
//! - **`:`** may appear in a member name (`…#Object..:`) but never at the
//!   front: an FQN begins with a repo-relative path, and this walk produces
//!   no path beginning with `external:`.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_haskell::extract::HsHeader;
use crate::track_haskell::project::HsProject;
use crate::track_haskell::resolve::HsScope;

/// The Haskell language. Stateless; only its associated types carry anything.
pub struct HsLang;

impl Language for HsLang {
    const LANG: Lang = Lang::Haskell;
    const DOMAIN: Domain = Domain::Haskell;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Haskell owns and this one cannot drift apart.
    ///
    /// `.lhs` is Haskell source and is deliberately **not** claimed: literate
    /// Haskell is Bird-tracked or LaTeX-delimited prose with code inside it,
    /// which the pinned grammar reads as Haskell only after a de-literation
    /// step nothing here performs. `.hs-boot`, `.hsc` and `.chs` are left
    /// unclaimed for the same reason — the first is a signature file, the
    /// other two are pre-processor inputs that *generate* the `.hs` a build
    /// compiles.
    fn extensions() -> &'static [&'static str] {
        Lang::Haskell.extensions()
    }

    /// Build output. `dist-newstyle` is cabal's, `dist` is v1-cabal's and
    /// `.stack-work` is stack's, and all three hold the *generated* modules of
    /// this package and the unpacked sources of its dependencies. Descending
    /// into one would index a dependency as if the repository had written it,
    /// inventing in-repository definitions that inflate the resolution rate.
    fn skip_dirs() -> &'static [&'static str] {
        &["dist-newstyle", "dist", ".stack-work"]
    }

    type Header = HsHeader;
    type Scope = HsScope;
    type Config = HsProject;
}

/// The character separating a module from the declarations inside it.
///
/// Split on **first** occurrence — see the module header for why a Haskell
/// member name may legally contain another.
pub const MEMBER: char = '#';

/// The module FQN of a repo-relative path: `src/Data/Aeson.hs` →
/// `src/Data/Aeson`.
///
/// Total, because every `.hs` file the walk reaches is a module whether or
/// not it declares a header — Haskell 2010 §5.1 makes a headerless file
/// `module Main` — and an import naming an empty file still resolves.
pub fn module_fqn(rel_path: &str) -> String {
    rel_path.strip_suffix(".hs").unwrap_or(rel_path).to_string()
}

/// The path, relative to a source root, that a dotted module name lives at:
/// `Data.Aeson.Key` → `Data/Aeson/Key`.
///
/// The inverse of [`module_fqn`] under a root, and GHC's own rule.
pub fn module_path(module_name: &str) -> String {
    module_name.replace('.', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haskell_reports_as_haskell_and_hashes_in_the_haskell_domain() {
        assert_eq!(HsLang::LANG, Lang::Haskell);
        assert_eq!(HsLang::DOMAIN, Domain::Haskell);
        assert_eq!(HsLang::LANG.domain(), HsLang::DOMAIN);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(HsLang::extensions(), Lang::Haskell.extensions());
        assert_eq!(HsLang::extensions(), ["hs"]);
        for unclaimed in ["lhs", "hs-boot", "hsc", "chs", "cabal"] {
            assert!(!HsLang::extensions().contains(&unclaimed));
        }
    }

    #[test]
    fn build_output_is_never_descended_into() {
        for dir in ["dist-newstyle", "dist", ".stack-work"] {
            assert!(HsLang::skip_dirs().contains(&dir));
        }
    }

    #[test]
    fn a_module_identity_is_its_path_and_a_name_maps_back_onto_one() {
        assert_eq!(module_fqn("src/Data/Aeson.hs"), "src/Data/Aeson");
        assert_eq!(module_path("Data.Aeson"), "Data/Aeson");
        // The six `module Main` files of the measured corpus: one declared
        // name, six identities, and only the path tells them apart.
        assert_ne!(
            module_fqn("tests/Tests.hs"),
            module_fqn("examples/src/Generic.hs"),
        );
        // A path that lost no suffix is still a module: the walk only offers
        // `.hs`, and a name that dropped one must not become another file's.
        assert_eq!(module_fqn("Setup"), "Setup");
    }

    #[test]
    fn the_external_prefix_is_unreachable_from_this_domain() {
        // `crate::pipeline` keys a dependency under `external:<pkg>` and rests
        // that on no FQN in the domain spelling one. A module FQN is a
        // repo-relative path, and a definition FQN is a path followed by `#`.
        assert!(!module_fqn("src/Data/Aeson.hs").starts_with("external:"));
        assert!(!module_fqn("external/Foo.hs").starts_with("external:"));
    }
}
