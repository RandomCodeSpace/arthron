//! Scala's [`Language`] impl: the constants the track is reported under, the
//! three types only Scala's own layers may read, and the FQN grammar every one
//! of them agrees on.
//!
//! # The FQN grammar
//!
//! ```text
//! _root_ ( '.' <container> )* ( '#' <member> ( '.' <member> )* )?
//! ```
//!
//! A **container** is a package or an `object` — the two things a path may be
//! written *through*. A **member** is everything else: a class, a trait, an
//! `enum`, a `type`, a `def`, a `val`, a `var`, a named `given`, an enum case.
//! `#` marks the one place the path crosses from the container namespace into
//! the declaration namespace, and after it every step joins with `.`.
//!
//! Three properties this buys, each of which a flat dotted name would lose:
//!
//! - **Companions stay two nodes.** `class Foo` is `…#Foo` and `object Foo` is
//!   `….Foo`, which is exactly Scala's own term/type split — a language where
//!   both may be written side by side in one file, and where merging them
//!   would silently make one declaration disappear into the other.
//! - **A path can be walked.** `import p.O.Inner` is `p` then `O` then
//!   `Inner`, and the resolver reads the shape rather than guessing where the
//!   package stops: at every step it probes the container key first and the
//!   member key second, which is the order Scala's own lookup takes.
//! - **`_root_` cannot be spelled by anything else.** It is a reserved word,
//!   so no package, object or class may be named it, and it is also what
//!   Scala itself calls the root package. Every FQN therefore starts at a
//!   name that exists and is unique, and none is ever empty — which keeps a
//!   file in the unnamed package from hashing to the same node as an absent
//!   name, and keeps [`crate::pipeline`]'s `external:` prefix unreachable.
//!
//! # The two container marks
//!
//! [`Definition::owner`](crate::model::Definition::owner) is one flat chain,
//! and it has to answer two questions the chain alone does not.
//!
//! **Which segments are containers?** `object O { class C }` is `….O#C` while
//! `class C { object O }` is `…#C.O`, so [`crate::lang::Resolver::def_fqn`]
//! must know. A container segment therefore carries [`CONTAINER_MARK`], a
//! `.`, which is not a character a Scala identifier may contain.
//!
//! **Which containers open a lookup scope?** Scala draws a line here that
//! costs a resolver dearly if it is missed:
//!
//! ```text
//! package a.b        // only a.b's members are in scope — NOT a's
//! package a
//! package b          // both a's and a.b's members are in scope
//! ```
//!
//! The measured corpus turns on it. `ujson/argonaut/…/ArgonautJson.scala`
//! writes `package ujson.argonaut` and then `import argonaut.{Json, …}`,
//! naming the *Argonaut library* — and there is a package called
//! `ujson.argonaut` one hop up. A resolver that put `ujson`'s members in
//! scope would bind `argonaut` to the in-repository package and either miss,
//! or, in a repository where that package held a `Json`, mint a confidently
//! wrong edge. So an intermediate segment of a qualified `package` clause
//! carries [`QUALIFIER_MARK`] instead: a container in the FQN, never a scope.
//!
//! `..` is the mark because it is `.` twice, and neither one nor two of them
//! can appear in a Scala identifier. The one exception is a back-quoted
//! identifier: ``val `a.b` = 1`` really can contain a dot. The marks never
//! leave this track — every FQN is composed by `def_fqn`, which strips them,
//! and no owner chain is stored — so the cost of that collision is confined
//! to a back-quoted *container* name beginning with a dot, which nothing in
//! the measured corpus writes.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_scala::extract::ScalaHeader;
use crate::track_scala::resolve::ScalaScope;

/// The Scala language. Stateless; only its associated types carry anything.
pub struct ScalaLang;

/// Phase 0 for Scala: deliberately empty.
///
/// Every other track built here reads a manifest before it reads a file,
/// because the manifest is where the language states a name the source does
/// not: Go's module path, Rust's crate roots, Ruby's load path, PHP's PSR-4
/// prefixes. **Scala states it in the source.** A file's package is its
/// `package` clause and nothing else — not its directory, not its source
/// root, not its build target — so a path is resolved against facts that are
/// all in the tree the walk already read.
///
/// That is not a shortcut around the measured corpus's hardest property. The
/// build there selects among 15 source-root names across 47 directories per
/// (Scala version, platform) combination, and 13 fully-qualified names are
/// each written in two or three of those roots. Reading the build would not
/// help a bit: mill's `PlatformScalaModule` picks a root per *build*, and a
/// scan measures the tree, so the honest graph holds the union over
/// configurations and records the duplicate declarations. `Resolver::mergeable`
/// is where that is said, and the corpus test is where it is counted.
///
/// So the digest is empty and a Scala scan is never invalidated by a manifest
/// — which is the contract [`crate::lang::Resolver::config_digest`] already
/// states for a language with no project manifest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScalaProject;

impl Language for ScalaLang {
    const LANG: Lang = Lang::Scala;
    const DOMAIN: Domain = Domain::Scala;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Scala owns and this one cannot drift apart.
    fn extensions() -> &'static [&'static str] {
        Lang::Scala.extensions()
    }

    /// Build output. mill writes `out/` and sbt writes `target/`, and both
    /// hold generated Scala — upickle's own `Generated.scala` is written into
    /// one by the build. Descending into either would index a generator's
    /// output, and a dependency's unpacked sources, as if the repository had
    /// written them, inventing in-repository definitions that inflate the
    /// resolution rate with links to code the repository does not own.
    fn skip_dirs() -> &'static [&'static str] {
        &["out", "target"]
    }

    type Header = ScalaHeader;
    type Scope = ScalaScope;
    type Config = ScalaProject;
}

/// Scala's own name for the root package, and the root of every FQN here.
///
/// A reserved word: no package, object, class or member may be called it, so
/// the prefix can never collide with a name the source writes.
pub const ROOT: &str = "_root_";

/// The prefix an owner-chain segment carries when it is a *container* that
/// opens a lookup scope — an `object`, a `package object`, or the last
/// segment of a `package` clause.
pub const CONTAINER_MARK: &str = ".";

/// The prefix an owner-chain segment carries when it is a container that
/// opens **no** lookup scope: an intermediate segment of a qualified
/// `package a.b` clause. See the module docs for the rule and for the
/// corpus site that turns on it.
pub const QUALIFIER_MARK: &str = "..";

/// Mark one owner-chain segment as a container that opens a scope.
pub fn mark(segment: &str) -> String {
    format!("{CONTAINER_MARK}{segment}")
}

/// Mark one owner-chain segment as a container that opens no scope.
pub fn mark_qualifier(segment: &str) -> String {
    format!("{QUALIFIER_MARK}{segment}")
}

/// The marked segments one dotted `package` clause contributes: every
/// segment is a container, and only the last opens a scope.
pub fn clause_segments(dotted: &str) -> Vec<String> {
    let names: Vec<&str> = dotted.split('.').collect();
    names
        .iter()
        .enumerate()
        .map(|(at, name)| {
            if at + 1 == names.len() {
                mark(name)
            } else {
                mark_qualifier(name)
            }
        })
        .collect()
}

/// Whether an owner-chain segment names a container.
pub fn is_container(segment: &str) -> bool {
    segment.starts_with(CONTAINER_MARK)
}

/// Whether an owner-chain segment names a container a simple name may be
/// looked up in.
pub fn opens_scope(segment: &str) -> bool {
    is_container(segment) && !segment.starts_with(QUALIFIER_MARK)
}

/// A segment with its container mark removed, if it had one.
pub fn unmark(segment: &str) -> &str {
    segment
        .strip_prefix(QUALIFIER_MARK)
        .or_else(|| segment.strip_prefix(CONTAINER_MARK))
        .unwrap_or(segment)
}

/// Append one segment to a partially composed FQN.
///
/// `members` is the composition's only state: once a member has been
/// appended, every later segment is a member of it, because Scala has no way
/// back out of a declaration into a package.
pub fn push_segment(fqn: &mut String, members: &mut bool, segment: &str, container: bool) {
    if *members || container {
        fqn.push('.');
    } else {
        fqn.push('#');
        *members = true;
    }
    fqn.push_str(segment);
}

/// Compose the FQN of a container chain: `_root_`, then one dotted segment
/// per (unmarked) name.
pub fn container_fqn(chain: &[String]) -> String {
    let mut out = String::from(ROOT);
    for segment in chain {
        out.push('.');
        out.push_str(unmark(segment));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scala_reports_as_scala_and_hashes_in_the_scala_domain() {
        assert_eq!(ScalaLang::LANG, Lang::Scala);
        assert_eq!(ScalaLang::DOMAIN, Domain::Scala);
        assert_eq!(ScalaLang::LANG.domain(), ScalaLang::DOMAIN);
        assert_eq!(ScalaLang::LANG.tier(), 2);
        assert_eq!(ScalaLang::LANG.rate_scope(), "import resolution");
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(ScalaLang::extensions(), Lang::Scala.extensions());
        assert_eq!(ScalaLang::extensions(), ["scala", "sc"]);
        // The tier-2 registration deliberately left `.sbt` unclaimed, and
        // going live claims nothing it had not.
        for unclaimed in ["sbt", "mill", "sc.scala"] {
            assert!(!ScalaLang::extensions().contains(&unclaimed));
        }
    }

    #[test]
    fn build_output_is_never_descended_into() {
        assert!(ScalaLang::skip_dirs().contains(&"out"));
        assert!(ScalaLang::skip_dirs().contains(&"target"));
    }

    #[test]
    fn a_container_chain_starts_at_the_root_package() {
        assert_eq!(container_fqn(&[]), "_root_");
        assert_eq!(
            container_fqn(&[mark("upickle"), mark("core")]),
            "_root_.upickle.core",
        );
        // Marked and unmarked segments compose the same chain: the mark is a
        // fact about the owner chain, never about the name.
        assert_eq!(
            container_fqn(&["upickle".to_string(), "core".to_string()]),
            "_root_.upickle.core",
        );
    }

    #[test]
    fn the_hash_appears_once_and_members_join_with_dots() {
        let mut fqn = container_fqn(&[mark("p")]);
        let mut members = false;
        push_segment(&mut fqn, &mut members, "O", true);
        assert_eq!(fqn, "_root_.p.O");
        push_segment(&mut fqn, &mut members, "C", false);
        assert_eq!(fqn, "_root_.p.O#C");
        // An `object` nested inside a class is a member like any other: the
        // container namespace does not reopen below a declaration.
        push_segment(&mut fqn, &mut members, "Inner", true);
        assert_eq!(fqn, "_root_.p.O#C.Inner");
    }

    #[test]
    fn a_companion_pair_is_two_identities() {
        let class = {
            let mut fqn = container_fqn(&[mark("p")]);
            let mut members = false;
            push_segment(&mut fqn, &mut members, "Foo", false);
            fqn
        };
        let object = {
            let mut fqn = container_fqn(&[mark("p")]);
            let mut members = false;
            push_segment(&mut fqn, &mut members, "Foo", true);
            fqn
        };
        assert_eq!(class, "_root_.p#Foo");
        assert_eq!(object, "_root_.p.Foo");
        assert_ne!(class, object);
    }

    #[test]
    fn the_mark_is_not_a_character_an_identifier_carries() {
        assert!(is_container(&mark("core")));
        assert!(!is_container("Visitor"));
        assert_eq!(unmark(&mark("core")), "core");
        assert_eq!(unmark(&mark_qualifier("upickle")), "upickle");
        assert_eq!(unmark("Visitor"), "Visitor");
        // Scala's operator identifiers are made of symbols — `::` is a real
        // object in the standard library — so a mark made of them would be
        // ambiguous. `.` is punctuation the grammar reserves.
        assert!(!is_container("::"));
        assert!(!is_container("+:"));
    }

    #[test]
    fn a_qualified_package_clause_opens_one_scope_and_not_three() {
        // `package a.b.c` puts only `a.b.c`'s members in scope. Every
        // segment is still a container of the FQN.
        let chain = clause_segments("a.b.c");
        assert_eq!(chain, ["..a", "..b", ".c"]);
        assert!(chain.iter().all(|s| is_container(s)));
        assert_eq!(
            chain.iter().filter(|s| opens_scope(s)).count(),
            1,
            "{chain:?}",
        );
        assert_eq!(container_fqn(&chain), "_root_.a.b.c");
    }

    #[test]
    fn separate_package_clauses_each_open_their_own_scope() {
        // `package a` then `package b` is the same package as `package a.b`
        // and a different set of scopes — which is the whole of the rule.
        let mut chain = clause_segments("a");
        chain.extend(clause_segments("b"));
        assert_eq!(chain, [".a", ".b"]);
        assert!(chain.iter().all(|s| opens_scope(s)));
        assert_eq!(
            container_fqn(&chain),
            container_fqn(&clause_segments("a.b"))
        );
    }
}
