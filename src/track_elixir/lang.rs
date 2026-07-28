//! Elixir's [`Language`] impl: the constants the track is reported under, the
//! three types only Elixir's own layers may read, and the FQN grammar every
//! one of them agrees on.
//!
//! # The FQN grammar
//!
//! ```text
//! <module> ( '#' ( <name> '/' <arity> | '%' <field> ) )?
//! ```
//!
//! A **module** is the whole dotted name, written exactly as the compiler
//! spells the atom it becomes: `Plug.Conn.Utils`. The dots are characters in
//! one name and not steps through containers — Elixir has no namespace
//! hierarchy, no package that owns a prefix, and no way to reopen `Plug` and
//! reach `Plug.Conn` from inside it. That is why
//! [`crate::track_elixir::resolve`] can probe a module name exactly rather
//! than searching for it, and why a name absent from the repository's own
//! module set is somebody else's with no third possibility.
//!
//! `#` marks the one place a path crosses from the module namespace into the
//! declaration namespace, and it is a character no Elixir module name or
//! function name may carry — it opens a comment. Below it:
//!
//! - A **function or macro** is `<name>/<arity>`, because that pair is what
//!   Elixir dispatches on: `Plug.Conn#put_resp_header/3`. Two clauses of one
//!   name and arity are one function and one node; `foo/1` and `foo/2` are
//!   two functions and two nodes, which a name alone would merge.
//! - A **struct field** is `%<key>`, because `defstruct`'s keys share the
//!   module with its functions and `%` is the character Elixir itself puts in
//!   front of a struct. A field can therefore never collide with a
//!   zero-arity function of the same name.
//!
//! Nothing here can spell a `:`, so [`crate::pipeline`]'s `external:` prefix
//! is unreachable from this domain: a module name is alphanumeric segments
//! joined by dots, and a function name is an identifier or an operator built
//! from the punctuation Elixir allows, which does not include a colon.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_elixir::extract::ElixirHeader;
use crate::track_elixir::resolve::ElixirScope;

/// The Elixir language. Stateless; only its associated types carry anything.
pub struct ElixirLang;

/// Phase 0 for Elixir: deliberately empty.
///
/// Most tracks here read a manifest before they read a file, because the
/// language states a name the source does not — Go's module path, Ruby's load
/// path, PHP's PSR-4 prefixes. **Elixir states it in the source and nowhere
/// else.** A module's name is the `defmodule` that declares it, composed
/// through the `defmodule`s that enclose it, and a reference names that same
/// atom; no directory, no source root and no build target enters it.
///
/// `mix.exs` is read as *source*, like any other `.exs` file the walk
/// reaches, and never as configuration. Its `deps` list names **packages** —
/// `:plug_crypto`, `:telemetry` — and a package does not give its modules:
/// `plug_crypto` supplies `Plug.Crypto.*`, `telemetry` supplies `:telemetry`,
/// and reading either mapping out of the manifest would buy a guess rather
/// than a fact. That is the lesson PHP wrote down for `guzzlehttp/promises`
/// and C# for `Microsoft.NET.Test.Sdk`, and this track does not repeat it.
///
/// So the digest is empty and an Elixir scan is never invalidated by a
/// manifest — which is the contract [`crate::lang::Resolver::config_digest`]
/// already states for a language with no project manifest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ElixirProject;

impl Language for ElixirLang {
    const LANG: Lang = Lang::Elixir;
    const DOMAIN: Domain = Domain::Elixir;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Elixir owns and this one cannot drift apart.
    fn extensions() -> &'static [&'static str] {
        Lang::Elixir.extensions()
    }

    /// Build output and fetched dependencies. `mix` unpacks every hex package
    /// into `deps/` and compiles into `_build/`, and both hold real `.ex`
    /// files somebody else wrote. Descending into either would index a
    /// dependency as if this repository had declared it — inventing
    /// in-repository modules that turn an external reference into a resolved
    /// one and inflate the rate with code the repository does not own.
    fn skip_dirs() -> &'static [&'static str] {
        &["_build", "deps"]
    }

    type Header = ElixirHeader;
    type Scope = ElixirScope;
    type Config = ElixirProject;
}

/// The character separating a module from a declaration inside it.
///
/// `#` opens a comment in Elixir, so it appears in no module name and in no
/// function name.
pub const MEMBER_MARK: char = '#';

/// The character marking a struct field, so that `%host` and `host/0` are
/// two identities under one module.
pub const FIELD_MARK: char = '%';

/// The dotted module name an owner chain and a declared name compose.
///
/// The owner chain holds the enclosing module's *already composed* name, so
/// `defmodule InvalidCSRFTokenError` inside `defmodule Plug.CSRFProtection`
/// composes `Plug.CSRFProtection.InvalidCSRFTokenError` — a name that appears
/// nowhere in the file that declares it.
pub fn module_fqn(owner: &[String], name: &str) -> String {
    if owner.is_empty() {
        return name.to_string();
    }
    format!("{}.{name}", owner.join("."))
}

/// A function or macro's key inside its module: `put_resp_header/3`.
pub fn function_key(name: &str, arity: u32) -> String {
    format!("{name}/{arity}")
}

/// A struct field's key inside its module: `%host`.
pub fn field_key(name: &str) -> String {
    format!("{FIELD_MARK}{name}")
}

/// A declaration's FQN: its module, the crossing, and its key.
pub fn member_fqn(module: &str, key: &str) -> String {
    format!("{module}{MEMBER_MARK}{key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elixir_reports_as_elixir_and_hashes_in_the_elixir_domain() {
        assert_eq!(ElixirLang::LANG, Lang::Elixir);
        assert_eq!(ElixirLang::DOMAIN, Domain::Elixir);
        assert_eq!(ElixirLang::LANG.domain(), ElixirLang::DOMAIN);
        assert_eq!(ElixirLang::LANG.tier(), 2);
        assert_eq!(ElixirLang::LANG.rate_scope(), "import resolution");
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(ElixirLang::extensions(), Lang::Elixir.extensions());
        assert_eq!(ElixirLang::extensions(), ["ex", "exs"]);
        // The tier-2 registration committed `.ex` and `.exs` and nothing
        // else; going live claims no extension it had not.
        for unclaimed in ["eex", "leex", "heex", "erl"] {
            assert!(!ElixirLang::extensions().contains(&unclaimed));
        }
    }

    #[test]
    fn fetched_dependencies_and_build_output_are_never_descended_into() {
        assert!(ElixirLang::skip_dirs().contains(&"deps"));
        assert!(ElixirLang::skip_dirs().contains(&"_build"));
    }

    #[test]
    fn a_nested_module_composes_a_name_the_source_never_writes() {
        assert_eq!(
            module_fqn(&[], "Plug.CSRFProtection"),
            "Plug.CSRFProtection"
        );
        assert_eq!(
            module_fqn(
                &["Plug.CSRFProtection".to_string()],
                "InvalidCSRFTokenError"
            ),
            "Plug.CSRFProtection.InvalidCSRFTokenError",
        );
    }

    #[test]
    fn a_function_is_named_by_its_arity_and_a_field_cannot_collide_with_one() {
        let module = "Plug.Conn";
        assert_eq!(
            member_fqn(module, &function_key("put_resp_header", 3)),
            "Plug.Conn#put_resp_header/3",
        );
        // Same name, two arities, two nodes — which is what Elixir dispatch
        // says and what a name alone would have merged.
        assert_ne!(
            member_fqn(module, &function_key("get", 1)),
            member_fqn(module, &function_key("get", 2)),
        );
        assert_ne!(
            member_fqn(module, &field_key("host")),
            member_fqn(module, &function_key("host", 0)),
        );
        assert_eq!(member_fqn(module, &field_key("host")), "Plug.Conn#%host");
    }

    #[test]
    fn no_identity_this_grammar_composes_can_spell_the_external_prefix() {
        // `external:` is minted by the driver for a dependency node, and it
        // must be unreachable from any candidate this track probes.
        for fqn in [
            module_fqn(&["Plug".to_string()], "Conn"),
            member_fqn("Plug.Conn", &function_key("get_req_header", 2)),
            member_fqn("Plug.Conn", &field_key("host")),
        ] {
            assert!(!fqn.contains(':'), "{fqn}");
        }
    }
}
