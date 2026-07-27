//! C#'s [`Language`] impl: the constants the track is reported under, the
//! three types only C#'s own layers may read, and the FQN grammar all of them
//! agree on.
//!
//! # The FQN grammar
//!
//! ```text
//! namespace  := Ident ("." Ident)*              Serilog.Core        ("" is the global namespace)
//! type       := namespace "#" Ident [arity]     Serilog.Core#Logger
//! nested     := type "+" Ident [arity]          Serilog.Context#EnricherStack+Enumerator
//! member     := type "::" Ident                 Serilog.Core#Logger::Name
//! method     := type "::" Ident "(" Type,* ")"  Serilog.Core#Logger::Write(LogEvent)
//! arity      := "`" digits                      Serilog.Data#LogEventPropertyValueVisitor`2
//! ```
//!
//! Four invariants, and the last two are C#'s own:
//!
//! 1. **`#` separates a container from its members, and a container's own
//!    name carries none** — the repository's convention, already true of Go's
//!    `{import path}#{Recv}.{name}`, Java's `{package}#{Type}` and PHP's
//!    `{namespace}#{Class}`. It is what keeps the namespace `A.B` and the
//!    type `B` of namespace `A` two identities, where C# spells both `A.B`.
//! 2. **`.` only joins namespace segments**, before the `#`; `+` only steps
//!    from a type to a type nested in it, which is how .NET metadata spells
//!    the same step; `::` only steps from a type to one of its members.
//! 3. **A type's arity is part of its name.** `Foo<T>` and `Foo<T, U>` are
//!    two types in one namespace, and C# itself separates them in metadata by
//!    the same backtick this does.
//! 4. **A method's parameter types are part of its key.** C# overloads on
//!    them, and a key without them would hash `Write(string)` and
//!    `Write(LogEvent)` to one node. The types are the *source spelling* —
//!    `int`, `ReadOnlySpan<object?>` — because tier 2 resolves no type and so
//!    has nothing to canonicalise one with. Two overloads that differ only in
//!    how their types are spelled would collide; C# has no such pair, since
//!    two spellings of one type in one signature list is a compile error.
//!
//! `#`, `+`, `` ` ``, `(`, `)`, `,` and `:` are all illegal in a C#
//! identifier, so no declared name can forge a separator.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_csharp::extract::CsHeader;
use crate::track_csharp::resolve::{CsProject, CsScope};

/// The C# language. Stateless; only its associated types carry anything.
pub struct CsLang;

impl Language for CsLang {
    const LANG: Lang = Lang::CSharp;
    const DOMAIN: Domain = Domain::CSharp;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what C# owns and this one cannot drift apart.
    fn extensions() -> &'static [&'static str] {
        Lang::CSharp.extensions()
    }

    /// The SDK's own output directories. `obj/` is where the build writes
    /// generated sources — `*.AssemblyInfo.cs`, `*.GlobalUsings.g.cs` — and
    /// descending into one would index generated code as if this repository
    /// had written it, inventing in-repository definitions the way PHP's
    /// `vendor/` and Python's `.venv` would. `bin/` holds the compiled output
    /// and any `.cs` in it is a copy of a source already read.
    fn skip_dirs() -> &'static [&'static str] {
        &["bin", "obj"]
    }

    type Header = CsHeader;
    type Scope = CsScope;
    type Config = CsProject;
}

/// Separates a container from its members. Never appears in a C# identifier.
pub const MEMBER: char = '#';

/// Steps from a type to a type nested inside it, as .NET metadata spells it.
pub const NESTED: char = '+';

/// Introduces a generic arity, as .NET metadata spells it.
pub const ARITY: char = '`';

/// The namespace FQN of a namespace name. The global namespace is the empty
/// string: a container with no name, which is a different fact from a file
/// naming no container.
pub fn namespace_fqn(name: &str) -> String {
    name.to_string()
}

/// The FQN of a type, given its namespace and its outer-to-inner name path.
///
/// Each element of `path` is a name that already carries its own arity, so a
/// nested generic type is spelled the way metadata spells it.
pub fn type_fqn(namespace: &str, path: &[String]) -> String {
    format!("{namespace}{MEMBER}{}", path.join(&NESTED.to_string()))
}

/// The FQN of a member of a type.
pub fn member_fqn(type_fqn: &str, key: &str) -> String {
    format!("{type_fqn}::{key}")
}

/// A type's name with its arity attached: `Visitor` at arity 0, ``Visitor`2``
/// at arity 2.
pub fn arity_name(name: &str, arity: usize) -> String {
    if arity == 0 {
        name.to_string()
    } else {
        format!("{name}{ARITY}{arity}")
    }
}

/// Every namespace a declaration of `name` implies, longest first: `A.B.C`
/// implies `A.B` and `A`.
///
/// `namespace A.B.C;` is shorthand for three nested namespace declarations,
/// so a file writing it declares all three and `using A.B;` names something
/// that file created. Without this a `using` of an intermediate namespace no
/// file spells on its own would be classified as living outside the
/// repository — an `External` for a name this repository does in fact
/// declare, which is the one misclassification that raises a rate for free.
///
/// This handles the *dotted* spelling only. C# writes the same nesting with
/// braces, and `crate::track_csharp::extract`'s `namespace_name` composes
/// that one before it ever reaches here.
pub fn implied_namespaces(name: &str) -> Vec<String> {
    let segments: Vec<&str> = name.split('.').collect();
    (1..segments.len())
        .rev()
        .map(|i| segments[..i].join("."))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csharp_reports_as_csharp_and_hashes_in_the_csharp_domain() {
        assert_eq!(CsLang::LANG, Lang::CSharp);
        assert_eq!(CsLang::DOMAIN, Domain::CSharp);
        assert_eq!(CsLang::LANG.domain(), CsLang::DOMAIN);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(CsLang::extensions(), Lang::CSharp.extensions());
        assert_eq!(CsLang::extensions(), ["cs"]);
        // `.csx` (a C# script) and `.razor`/`.cshtml` (a template with C# in
        // it) stay unclaimed: the extension list was committed with the
        // tier-2 registration, and the honest moment to widen it is a commit
        // that measures the files it adds.
        for ext in ["csx", "razor", "cshtml", "vb"] {
            assert!(!CsLang::extensions().contains(&ext));
        }
    }

    #[test]
    fn the_builds_own_output_is_never_descended_into() {
        assert!(CsLang::skip_dirs().contains(&"obj"));
        assert!(CsLang::skip_dirs().contains(&"bin"));
    }

    #[test]
    fn a_namespace_and_a_type_of_the_same_spelling_are_two_identities() {
        // C# writes both `Serilog.Core`. The `#` is what keeps them apart.
        assert_eq!(namespace_fqn("Serilog.Core"), "Serilog.Core");
        assert_eq!(
            type_fqn("Serilog", &["Core".to_string()]),
            "Serilog#Core".to_string(),
        );
        assert_eq!(
            type_fqn(
                "Serilog.Context",
                &["EnricherStack".into(), "Enumerator".into()]
            ),
            "Serilog.Context#EnricherStack+Enumerator".to_string(),
        );
        assert_eq!(
            member_fqn(&type_fqn("N", &["C".to_string()]), "Write(string)"),
            "N#C::Write(string)".to_string(),
        );
    }

    #[test]
    fn arity_is_part_of_a_generic_types_name() {
        assert_eq!(arity_name("Logger", 0), "Logger");
        assert_eq!(arity_name("Visitor", 2), "Visitor`2");
    }

    #[test]
    fn a_namespace_declaration_implies_every_namespace_above_it() {
        assert_eq!(
            implied_namespaces("Serilog.Settings.KeyValuePairs"),
            ["Serilog.Settings", "Serilog"],
        );
        assert!(implied_namespaces("Serilog").is_empty());
        assert!(implied_namespaces("").is_empty());
    }
}
