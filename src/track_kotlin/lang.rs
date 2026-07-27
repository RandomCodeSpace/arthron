//! Kotlin's [`Language`] impl: the constants the track is reported under, the
//! three types only Kotlin's own layers may read, and the FQN grammar every
//! one of them agrees on.
//!
//! # The FQN grammar
//!
//! ```text
//! package    := Ident ("." Ident)*                 okio, okio.internal   ("" is the default package)
//! classifier := package "#" Ident ("." Ident)*     okio#Buffer, okio#Path.Companion
//! callable   := package "#" [chain "."] Ident "()" okio#checkOffsetAndCount(), okio#Path.Companion.toPath()
//! value      := package "#" [chain "."] Ident "!"  okio#TestUtil.SEGMENT_SIZE!, okio#Buffer.head!
//! ```
//!
//! Three invariants:
//!
//! 1. **`#` separates a container from its members, and a container's own
//!    name carries none** — the repository's convention, already true of Go's
//!    `{import path}#{Recv}.{name}`, Java's `{package}#{Type}` and PHP's
//!    `{namespace}#{Class}`. It is what keeps the package `okio.internal` and
//!    a classifier `internal` of package `okio` two identities, where Kotlin
//!    spells both `okio.internal`.
//! 2. **`.` joins package segments before the `#`, and declaration-nesting
//!    steps after it.** Only the *last* step after the `#` may be a callable,
//!    because a declaration inside a function body is not a node — so the
//!    chain is unambiguous without a second separator.
//! 3. **A sigil says which of Kotlin's declaration spaces a name lives in.**
//!    Nothing for a classifier, `()` for a function, `!` for a property or a
//!    constant. Kotlin lets one package hold a class `Foo`, a function
//!    `Foo()` and a property `Foo` at once — a factory function beside the
//!    type it builds is idiomatic — and without the sigils the three would
//!    hash to one node.
//!
//! `#`, `(`, `)` and `!` are the reserved characters. A plain Kotlin
//! identifier cannot contain any of them, and a backtick-quoted one carries
//! its backticks into the name, so no declaration can spell a key another
//! declaration owns. `:` is reserved by [`crate::pipeline`] for its
//! `external:` prefix and appears in no Kotlin name: the JVM forbids it in a
//! backtick identifier and a package segment cannot hold it either.
//!
//! # What the identity space deliberately does **not** carry
//!
//! **The source set.** okio declares `okio.Lock` in `commonMain` as `expect`
//! and again in five platform source sets as `actual`; `okio.IOException` is
//! an `expect class` in one source set and an `actual typealias` in another.
//! Those are one entity — `import okio.Lock` names the same thing whichever
//! platform compiles — so they share one identity and the node carries one
//! declaration site per source set. Minting a node per source set would
//! answer "what is `okio.Lock`?" with six nodes that no reference
//! distinguishes.
//!
//! **A callable's signature.** A callable key is a *name*, not a name plus an
//! arity: an `import` names a member name and states no arity, and tier 2
//! emits no call site that could state one. So two overloads of one name are
//! one node here, with two declaration sites. That is the honest granularity
//! for what tier 2 measures, and it is the first thing a tier-1 Kotlin track
//! would have to refine — the way Java's `name/argc` key already does.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_kotlin::extract::KtHeader;
use crate::track_kotlin::resolve::KtScope;

/// The Kotlin language. Stateless; only its associated types carry anything.
pub struct KtLang;

impl Language for KtLang {
    const LANG: Lang = Lang::Kotlin;
    const DOMAIN: Domain = Domain::Kotlin;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Kotlin owns and this one cannot drift apart.
    ///
    /// `.kts` is claimed alongside `.kt`: a Gradle build script is Kotlin
    /// whose top level happens to be statements rather than declarations, one
    /// grammar reads both, and the ancestor allow-list in
    /// [`crate::track_kotlin::extract`] is what keeps a declaration written
    /// inside a script's configuration block from becoming a node.
    fn extensions() -> &'static [&'static str] {
        Lang::Kotlin.extensions()
    }

    /// `build/` is Gradle's output tree and `.gradle/` its cache. Descending
    /// into either would index generated and downloaded sources as if this
    /// repository had written them, inventing in-repository definitions that
    /// inflate the resolution rate — the hazard Rust's `target/` and PHP's
    /// `vendor/` carry.
    fn skip_dirs() -> &'static [&'static str] {
        &["build", ".gradle"]
    }

    type Header = KtHeader;
    type Scope = KtScope;
    /// Nothing. Kotlin states a declaration's container in the source that
    /// declares it, so no manifest decides an identity here and there is no
    /// phase 0 to run — which is why this track has no `project` module. A
    /// Gradle build file names artifacts and source sets; it names no
    /// package, and a package name is the whole of what an import resolves
    /// against.
    type Config = ();
}

/// Separates a container from its members. Never appears in a package name.
pub const MEMBER: char = '#';

/// The suffix a function's key carries. `(` and `)` are illegal in a Kotlin
/// identifier, backtick-quoted or not.
pub const CALLABLE: &str = "()";

/// The suffix a property's or constant's key carries.
pub const VALUE: char = '!';

/// The name an unnamed `companion object` is declared under, which is the
/// name Kotlin itself gives it and the one `import okio.ByteString.Companion.encodeUtf8`
/// spells.
pub const COMPANION: &str = "Companion";

/// The name a constructor is a member under. Kotlin has no identifier
/// spelling this — `<` and `>` are illegal even inside backticks — so it can
/// never collide with a declared function.
pub const INIT: &str = "<init>";

/// The segment an on-demand import carries in place of a member name.
///
/// `*` is not an identifier in any spelling, so a path ending in one is
/// unambiguously `import okio.*` rather than a member called `*`.
pub const ON_DEMAND: &str = "*";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kotlin_reports_as_kotlin_and_hashes_in_the_kotlin_domain() {
        assert_eq!(KtLang::LANG, Lang::Kotlin);
        assert_eq!(KtLang::DOMAIN, Domain::Kotlin);
        assert_eq!(KtLang::LANG.domain(), KtLang::DOMAIN);
        // Kotlin is not folded into `Jvm`: sharing a domain asserts that a
        // `.kt` import can name a `.java` definition in one reference space,
        // and nothing here has measured that.
        assert_ne!(KtLang::DOMAIN, Domain::Jvm);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(KtLang::extensions(), Lang::Kotlin.extensions());
        assert_eq!(KtLang::extensions(), ["kt", "kts"]);
        // Going live widens nothing: `.ktm` and the rest stay unclaimed.
        for unclaimed in ["ktm", "java", "gradle"] {
            assert!(!KtLang::extensions().contains(&unclaimed));
        }
    }

    #[test]
    fn gradles_output_trees_are_never_descended_into() {
        assert!(KtLang::skip_dirs().contains(&"build"));
        assert!(KtLang::skip_dirs().contains(&".gradle"));
    }

    #[test]
    fn the_reserved_characters_are_illegal_in_a_kotlin_name() {
        // The grammar's whole safety argument, restated where it is used.
        for reserved in [MEMBER, VALUE, ':'] {
            assert!(!reserved.is_alphanumeric());
        }
        assert_eq!(CALLABLE, "()");
        assert!(INIT.starts_with('<'));
        assert_eq!(ON_DEMAND, "*");
    }
}
