//! The one place a Kotlin [`crate::Outcome`] is produced. Never drops.
//!
//! # The gate is an import-resolution rate
//!
//! Kotlin is **tier 2** here: definitions, structure and imports, and no
//! verified call edges. The references this resolver classifies are exactly
//! the `import` headers the extractor emits, and the rate the gate defends is
//! the share of them that name something this repository declares. Every one
//! ends `Resolved`, `External`, or `Unresolved` with a reason.
//!
//! # The import model, as measured
//!
//! A Kotlin import names a **member of a package**, and the source is where
//! a package is declared — 226 of the corpus's 269 files write `package
//! okio`, four package names exist in the whole tree, and no manifest states
//! any of them. So resolution is a two-step lookup and needs no phase 0:
//!
//! 1. **Find the longest prefix of the path that this repository declares as
//!    a package.** That is Kotlin's own rule, and it is a probe rather than a
//!    guess: every file emits a node for the package it declares, so the
//!    package table is exactly the set of packages the walk found.
//!    The prefix stops one segment short of the end for a named import — an
//!    import must name something *in* a package — and may run to the end for
//!    an on-demand one, which names the package itself.
//! 2. **Probe the remaining segments as a declaration chain**, in the three
//!    keyspaces `lang.rs` defines: classifier, then callable, then value.
//!    Three probes because a Kotlin import names *a name*, not a declaration
//!    — `import okio.Buffer` imports the class, the factory function and the
//!    property if all three exist — so the site does not say which table it
//!    reads and this resolver must ask each.
//!
//! Step 1 runs longest-prefix first, which is the order Kotlin resolves in,
//! and every probe is recorded — the misses as well as the hits — so an edit
//! that later declares one of those names wakes the reference.
//!
//! # What each miss means
//!
//! - **No prefix of the path is a package this repository declares** ⇒
//!   [`crate::Outcome::External`], named by the path's root segment. This is
//!   the strongest in-repository claim available: the repository states its
//!   own packages in its own source, so "no package here could hold this
//!   name" is a fact read off the corpus rather than inferred from a
//!   manifest nobody read.
//! - **A package prefix is declared here and the path reached a declaration
//!   this build holds before it ran out** ⇒
//!   [`crate::UnresolvedReason::NoMatchingDefinition`]. The bucket reserved
//!   for meaning *our* bug. A one-segment chain always takes it: the package
//!   is held in full and the member is simply absent.
//! - **A package prefix is declared here and the path leaves everything this
//!   build holds at its *second* segment** ⇒
//!   [`crate::UnresolvedReason::UnknownPackage`]. `import okio.internal.linux.statx`
//!   is the measured case: `okio.internal` is declared here, nothing in it is
//!   called `linux`, and the real package `okio.internal.linux` is generated
//!   by cinterop from headers the corpus excludes. Reporting that as
//!   arthron's own missing definition would blame this extractor for a
//!   package that was never in the tree — the misattribution the Java review
//!   round called out one layer down.
//!
//! Both misses count **against** the rate. Neither is `External`, which is
//! the point: a package this build cannot see is not a package it may wave
//! out of the denominator.
//!
//! # Why the external rule is the root segment, and what it costs
//!
//! `External` sits outside **both** terms of the resolution rate, so widening
//! it is the cheapest way there is to raise a rate without linking anything.
//! The rule above is the narrowest one that is still true: an import under a
//! package this repository declares can never take it, so nothing that ought
//! to resolve can be laundered out of the denominator. What the *name* on the
//! node costs is precision, not honesty — `import org.junit.Test` and
//! `import org.assertj.core.api.Assertions` both land on `org`, because a
//! Kotlin import states no boundary between the package and the type and
//! this build reads no dependency's sources to find one. The root segment is
//! the coarsest unit that can be named without guessing where a dependency's
//! package begins, which is the same answer PHP's track gives for the same
//! reason.
//!
//! # `LocalBinding` does not apply here
//!
//! Tier 2 emits no expression-level reference, so no Kotlin reference can
//! name a parameter, a local or a receiver. The bucket stays empty and the
//! baseline records it as zero — which makes this track's rate un-gameable by
//! the one reclassification the rate's own definition permits.
//!
//! # Known non-claims
//!
//! - **On-demand imports are unexercised.** The corpus contains no
//!   `import okio.*`; the rule above is written for one and no measurement
//!   checks it. A second Kotlin corpus is what would earn that.
//! - **An import alias binds nothing here.** `import java.nio.file.Path as NioPath`
//!   creates a file-local name, and the references that would use it are
//!   expression-level ones tier 2 does not emit. The alias is carried in the
//!   reference's `raw_target` so two aliases of one target stay two rows.
//! - **`@file:JvmName` renames nothing in this space.** It changes what a
//!   Java caller sees; no Kotlin import spells the renamed form.

use std::collections::HashMap;
use std::path::Path;

use crate::lang::{
    Extractor, FileFacts, FileIndex, LayoutError, Resolution, Resolver, SymbolProbe,
};
use crate::model::{DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, Reference, node_id};
use crate::track_kotlin::extract::{KtExtractor, KtHeader};
use crate::track_kotlin::lang::{CALLABLE, KtLang, MEMBER, ON_DEMAND, VALUE};
use crate::{Outcome, UnresolvedReason};

use crate::lang::Language;

/// Kotlin's per-file scope: nothing.
///
/// Not an oversight and not a placeholder. The only reference kind this track
/// emits is the import itself, and a Kotlin import names a package-qualified
/// path, so there is no file-local environment to read it against. The
/// bindings an import *creates* — including its alias — matter only to the
/// expression-level references tier 2 does not emit; the day Kotlin goes to
/// tier 1, this is where they land.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KtScope;

/// The Kotlin resolver.
pub struct KtResolver;

/// The FQN of a package. The default package is the empty string: a
/// container with no name, which is a different fact from a file naming no
/// container.
fn package_fqn(segments: &[String]) -> String {
    segments.join(".")
}

/// The FQN of a classifier reached by `chain` inside `package`.
fn classifier_fqn(package: &str, chain: &[String]) -> String {
    format!("{package}{MEMBER}{}", chain.join("."))
}

/// The symbol table, plus the log of every identity this resolution asked it
/// about.
///
/// One value rather than two parameters, because the two are one obligation:
/// [`Resolution::candidates`] must list exactly what was probed and nothing
/// else, or the invalidation index it feeds would wake this reference for
/// edits that could not change its outcome.
struct Probes<'a> {
    table: &'a dyn SymbolProbe,
    seen: Vec<NodeId>,
}

impl Probes<'_> {
    /// Probe one FQN, recording it either way.
    fn hit(&mut self, fqn: &str) -> bool {
        let id = Probes::id(fqn);
        self.seen.push(id);
        self.table.probe(&id).is_some()
    }

    fn id(fqn: &str) -> NodeId {
        node_id(KtLang::DOMAIN, fqn)
    }
}

impl KtResolver {
    /// The two-step lookup, in the order the module header states.
    fn resolve_import(r: &Reference, p: &mut Probes) -> Outcome<NodeId, String> {
        let segments = &r.target.segments;
        let (on_demand, path) = match segments.split_last() {
            Some((last, rest)) if last == ON_DEMAND => (true, rest),
            _ => (false, segments.as_slice()),
        };
        let Some(root) = path.first() else {
            // `import *` is not Kotlin, and the extractor emits no reference
            // for a header with no path. Nothing was understood, so nothing
            // is claimed.
            return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition);
        };
        // A named import must leave at least one segment for the name it
        // imports; an on-demand import may name the package outright.
        // Neither may take the *default* package: Kotlin cannot import a
        // top-level declaration that sits in it, so a one-segment named
        // import names nothing this repository could declare.
        let longest = if on_demand {
            path.len()
        } else {
            path.len().saturating_sub(1)
        };

        let mut declared_here = false;
        let mut entered = false;
        for take in (1..=longest).rev() {
            let package = package_fqn(&path[..take]);
            if !p.hit(&package) {
                continue;
            }
            declared_here = true;
            let chain = &path[take..];
            if chain.is_empty() {
                // `import okio.*` names the package itself.
                return Outcome::Resolved(Probes::id(&package));
            }
            let base = classifier_fqn(&package, chain);
            if p.hit(&base) {
                return Outcome::Resolved(Probes::id(&base));
            }
            if !on_demand {
                // `import okio.Path.Companion.*` names a classifier's members;
                // a callable has none to import, so the two probes below are
                // only asked of a named import.
                let callable = format!("{base}{CALLABLE}");
                if p.hit(&callable) {
                    return Outcome::Resolved(Probes::id(&callable));
                }
                let value = format!("{base}{VALUE}");
                if p.hit(&value) {
                    return Outcome::Resolved(Probes::id(&value));
                }
            }
            // Where the path leaves what this build holds, which is what
            // tells the two misses apart. A one-segment chain names a direct
            // member of a package held in full; a longer one is inside a
            // declaration only if its head is one.
            if chain.len() == 1 || p.hit(&classifier_fqn(&package, &chain[..1])) {
                entered = true;
            }
        }
        match (declared_here, entered) {
            (false, _) => Outcome::External(root.clone()),
            (_, true) => Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
            (_, false) => Outcome::Unresolved(UnresolvedReason::UnknownPackage),
        }
    }
}

impl Resolver<KtLang> for KtResolver {
    /// Phase 0 reads nothing, because nothing outside the source decides a
    /// Kotlin identity. A Gradle build file names artifacts, plugins and
    /// source sets; a package name is written in the `.kt` file itself, and
    /// a package name is the whole of what an import resolves against.
    fn config(&self, _root: &Path, _files: &FileIndex) -> Result<(), LayoutError> {
        Ok(())
    }

    /// Empty, and never invalidated by this: there is no manifest whose
    /// change would re-root a Kotlin identity.
    fn config_digest(&self, _cfg: &()) -> Vec<u8> {
        Vec::new()
    }

    /// `None`: a Kotlin identity is decided by the `package` the file itself
    /// declares, so both phases build the same names from the same bytes and
    /// there is nothing for the driver to fold in from the store.
    fn declared_container(&self, _cfg: &(), _header: &KtHeader) -> Option<(String, String)> {
        None
    }

    /// Nothing to learn, for the reason [`Resolver::declared_container`]
    /// gives.
    fn learn_containers(&self, _cfg: &mut (), _names: &HashMap<String, String>) {}

    /// Every file the walk reached. Gradle's nested `build.gradle.kts` files
    /// are this repository's own build code, not a fence: okio's three
    /// modules share one package, so a per-module fence would make
    /// `okio-testing-support`'s helpers invisible to the module they were
    /// written for. What is genuinely not ours is `build/`, and that is
    /// pruned from the walk by [`KtLang::skip_dirs`] rather than filtered out
    /// of it.
    fn owns_file(&self, _cfg: &(), _rel_path: &str) -> bool {
        true
    }

    fn def_fqn(
        &self,
        _cfg: &(),
        _header: &KtHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        // The package names itself. It carries no `#`, which is what keeps
        // the package `okio.internal` and a classifier `internal` of package
        // `okio` two identities.
        if def.kind == DefKind::Module {
            return Some(Fqn::new(def.name.clone()));
        }
        // Everything else carries its package as `owner[0]` and its
        // classifier nesting after it.
        let package = owner.first()?;
        let mut chain: Vec<String> = owner[1..].to_vec();
        if def.name.is_empty() {
            return None;
        }
        chain.push(def.name.clone());
        let base = classifier_fqn(package, &chain);
        Some(Fqn::new(match def.kind {
            DefKind::Type | DefKind::Alias => base,
            DefKind::Function | DefKind::Method | DefKind::Constructor => {
                format!("{base}{CALLABLE}")
            }
            DefKind::Property | DefKind::Const | DefKind::Field | DefKind::Var => {
                format!("{base}{VALUE}")
            }
            // A shape the extractor does not produce. Not nameable, so not a
            // node, rather than named by a rule nobody wrote.
            DefKind::Module => return None,
        }))
    }

    /// Empty: a Kotlin node is reachable by exactly one identity. There are
    /// no export aliases — `import okio.Buffer as B` binds `B` in one file
    /// and is nameable from nowhere else — and an overload set is not a
    /// separate key, because a callable key is already a name rather than a
    /// signature.
    fn index_keys(&self, _cfg: &(), _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        Vec::new()
    }

    /// Two Kotlin declarations sharing an FQN are one entity declared for
    /// several platforms, or one importable name with several overloads.
    ///
    /// Neither is corruption, and both are ordinary in a multiplatform
    /// library: okio declares `okio.Lock` once as `expect` and five times as
    /// `actual`, and `okio.IOException` as an `expect class` in one source
    /// set and an `actual typealias` in another — the same name, the same
    /// import, different bodies. The identity space carries no source-set
    /// dimension by design, so those are one node with one declaration site
    /// per source set rather than six nodes no reference distinguishes.
    ///
    /// The test is name and owner, not kind: a `typealias` really is what a
    /// `class` of that name resolves to on the platform that writes it. What
    /// the FQN grammar already keeps apart — a classifier from a function
    /// from a property — can never reach this question, because the three
    /// carry different keys.
    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        a.name == b.name && a.owner == b.owner
    }

    fn scope(&self, _cfg: &(), _file: &FileFacts<KtLang>, _probe: &dyn SymbolProbe) -> KtScope {
        KtScope
    }

    /// Empty. Tier 2 emits no inheritance reference, so there is no supertype
    /// relation to derive and no member lookup that would walk one.
    fn link_kinds(&self) -> &'static [RefKind] {
        &[]
    }

    fn resolve(
        &self,
        _cfg: &(),
        _scope: &KtScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let mut p = Probes {
            table: probe,
            seen: Vec::new(),
        };
        let outcome = match r.kind {
            RefKind::Import => Self::resolve_import(r, &mut p),
            // Structurally unreachable: this track's extractor emits one
            // reference kind. Kept because `resolve` is total over
            // `Reference`, and the honest answer for a site a tier-2 language
            // does not link is the reason named for exactly that.
            _ => Outcome::Unresolved(UnresolvedReason::TierTwoLanguage),
        };
        Resolution {
            outcome,
            candidates: p.seen,
        }
    }
}

/// The Kotlin track's scan entry point, reading every `.kt` and `.kts` the
/// walk finds.
pub fn scan_kotlin(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_kotlin_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_kotlin`] under a repository's include/exclude globs. What
/// [`crate::track_kotlin::TRACK`] holds.
pub fn scan_kotlin_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<KtLang>(root, db, &KtExtractor, &KtResolver, filter)
}

/// Kotlin's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(KtLang::LANG, Lang::Kotlin));
    assert!(matches!(KtLang::DOMAIN, Domain::Kotlin));
};

/// The extractor's `Extractor` impl is what the driver runs; `extract` is
/// what the fixtures call. Naming both keeps the trait object honest.
const _: fn() = || {
    fn assert_extractor<T: Extractor<KtLang>>() {}
    assert_extractor::<KtExtractor>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeclSpace, DefFacets, RefTarget, Span, TargetRoot};
    use crate::track_kotlin::extract::extract;
    use std::collections::HashSet;

    /// A symbol table holding exactly these FQNs.
    fn table(fqns: &[&str]) -> HashSet<NodeId> {
        fqns.iter().map(|f| Probes::id(f)).collect()
    }

    /// The one import reference a source states.
    fn only_import(source: &str) -> Reference {
        let facts = extract("src/A.kt", source);
        assert_eq!(facts.refs.len(), 1, "{:?}", facts.refs);
        facts.refs.into_iter().next().expect("one import")
    }

    fn resolve(source: &str, known: &[&str]) -> Resolution {
        let t = table(known);
        KtResolver.resolve(&(), &KtScope, &only_import(source), &t)
    }

    fn def(kind: DefKind, name: &str, owner: &[&str]) -> Definition {
        Definition {
            kind,
            name: name.to_string(),
            owner: owner.iter().map(|o| (*o).to_string()).collect(),
            space: DeclSpace::Value,
            facets: DefFacets::default(),
            params: None,
            span: Span {
                byte_start: 0,
                byte_end: 0,
                line: 1,
            },
        }
    }

    fn fqn(kind: DefKind, name: &str, owner: &[&str]) -> String {
        let t: HashSet<NodeId> = HashSet::new();
        KtResolver
            .def_fqn(
                &(),
                &KtHeader::default(),
                &owner.iter().map(|o| (*o).to_string()).collect::<Vec<_>>(),
                &def(kind, name, owner),
                &t,
            )
            .map(Fqn::into_string)
            .unwrap_or_else(|| panic!("{kind:?} {name} is not nameable"))
    }

    #[test]
    fn a_package_and_a_classifier_of_the_same_written_name_are_two_identities() {
        // Kotlin spells both `okio.internal`. Hashing that string once would
        // merge the package node with the classifier node.
        let package = fqn(DefKind::Module, "okio.internal", &[]);
        let classifier = fqn(DefKind::Type, "internal", &["okio"]);
        assert_eq!(package, "okio.internal");
        assert_eq!(classifier, "okio#internal");
        assert_ne!(package, classifier);
    }

    #[test]
    fn the_three_declaration_spaces_take_three_keys() {
        // Kotlin permits a class `Foo`, a factory function `Foo()` and a
        // property `Foo` in one package at once.
        assert_eq!(fqn(DefKind::Type, "Foo", &["okio"]), "okio#Foo");
        assert_eq!(fqn(DefKind::Function, "Foo", &["okio"]), "okio#Foo()");
        assert_eq!(fqn(DefKind::Property, "Foo", &["okio"]), "okio#Foo!");
        assert_eq!(fqn(DefKind::Const, "Foo", &["okio"]), "okio#Foo!");
        // An alias takes the classifier key, because an `actual typealias` is
        // the same name an `expect class` declares one source set over.
        assert_eq!(fqn(DefKind::Alias, "Foo", &["okio"]), "okio#Foo");
    }

    #[test]
    fn a_nested_chain_is_joined_after_the_member_separator() {
        assert_eq!(
            fqn(DefKind::Method, "toPath", &["okio", "Path", "Companion"]),
            "okio#Path.Companion.toPath()",
        );
        assert_eq!(
            fqn(DefKind::Type, "Companion", &["okio", "Path"]),
            "okio#Path.Companion",
        );
        assert_eq!(
            fqn(DefKind::Constructor, "<init>", &["okio", "Buffer"]),
            "okio#Buffer.<init>()",
        );
    }

    #[test]
    fn the_default_package_is_a_container_with_no_name() {
        assert_eq!(fqn(DefKind::Module, "", &[]), "");
        // And a declaration in it still carries the separator, so it can
        // never be confused with a package called `Foo`.
        assert_eq!(fqn(DefKind::Type, "Foo", &[""]), "#Foo");
    }

    #[test]
    fn an_import_resolves_against_the_longest_declared_package_prefix() {
        let r = resolve(
            "package p\nimport okio.internal.HashFunction\n",
            &["okio", "okio.internal", "okio.internal#HashFunction"],
        );
        assert_eq!(
            r.outcome,
            Outcome::Resolved(Probes::id("okio.internal#HashFunction")),
        );
        // The longest prefix is probed first, which is Kotlin's own rule.
        assert_eq!(r.candidates[0], Probes::id("okio.internal"));
    }

    #[test]
    fn an_import_of_a_top_level_function_finds_it_in_the_callable_space() {
        let r = resolve(
            "package p\nimport okio.internal.commonWrite\n",
            &["okio.internal", "okio.internal#commonWrite()"],
        );
        assert_eq!(
            r.outcome,
            Outcome::Resolved(Probes::id("okio.internal#commonWrite()")),
        );
        // The classifier key is probed first and recorded even though it
        // missed: declaring `class commonWrite` later must wake this row.
        assert!(
            r.candidates
                .contains(&Probes::id("okio.internal#commonWrite"))
        );
    }

    #[test]
    fn an_import_of_a_companion_member_walks_the_whole_chain() {
        for (source, key) in [
            (
                "package p\nimport okio.Path.Companion.toPath\n",
                "okio#Path.Companion.toPath()",
            ),
            (
                "package p\nimport okio.TestUtil.SEGMENT_SIZE\n",
                "okio#TestUtil.SEGMENT_SIZE!",
            ),
        ] {
            let r = resolve(source, &["okio", key]);
            assert_eq!(r.outcome, Outcome::Resolved(Probes::id(key)), "{source}");
        }
    }

    #[test]
    fn no_declared_package_prefix_means_the_import_is_outside_this_repository() {
        let r = resolve("package p\nimport java.io.IOException\n", &["okio"]);
        assert_eq!(r.outcome, Outcome::External("java".to_string()));
        // Both prefixes were probed, so a file that later declares
        // `package java.io` wakes this row.
        assert_eq!(r.candidates, [Probes::id("java.io"), Probes::id("java")],);
    }

    #[test]
    fn a_one_segment_import_names_the_default_package_which_kotlin_cannot_import_from() {
        // No prefix is left for a package, so nothing in this repository
        // could be what it names.
        let r = resolve("package p\nimport Foo\n", &["", "#Foo"]);
        assert_eq!(r.outcome, Outcome::External("Foo".to_string()));
        assert!(r.candidates.is_empty(), "{:?}", r.candidates);
    }

    #[test]
    fn a_name_absent_from_a_package_held_in_full_is_arthrons_own_miss() {
        let r = resolve(
            "package p\nimport okio.internal.readString\n",
            &["okio.internal"],
        );
        assert_eq!(
            r.outcome,
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
        );
    }

    #[test]
    fn a_miss_inside_a_classifier_this_build_holds_is_arthrons_own_miss() {
        // `okio#ByteString` is here and its companion is not, which is the
        // measured shape of the grammar defect.
        let r = resolve(
            "package p\nimport okio.ByteString.Companion.encodeUtf8\n",
            &["okio", "okio#ByteString"],
        );
        assert_eq!(
            r.outcome,
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
        );
    }

    #[test]
    fn a_path_that_leaves_this_build_at_its_second_segment_names_a_package_nobody_indexed() {
        // `okio.internal.linux` is generated by cinterop from headers the
        // corpus excludes: `okio.internal` is declared here, nothing in it is
        // called `linux`, and blaming this extractor for that would be
        // reporting a package that was never in the tree as our own bug.
        let r = resolve(
            "package p\nimport okio.internal.linux.statx\n",
            &["okio", "okio.internal"],
        );
        assert_eq!(
            r.outcome,
            Outcome::Unresolved(UnresolvedReason::UnknownPackage),
        );
        // And it is *not* external: a package this build cannot see is not
        // one it may wave out of the denominator.
        assert!(!matches!(r.outcome, Outcome::External(_)));
    }

    #[test]
    fn an_on_demand_import_names_the_container_to_its_left() {
        // Unexercised by the corpus, which writes none; the rule is still
        // written down and checked.
        let package = resolve("package p\nimport okio.*\n", &["okio"]);
        assert_eq!(package.outcome, Outcome::Resolved(Probes::id("okio")));
        let classifier = resolve(
            "package p\nimport okio.Path.Companion.*\n",
            &["okio", "okio#Path.Companion"],
        );
        assert_eq!(
            classifier.outcome,
            Outcome::Resolved(Probes::id("okio#Path.Companion")),
        );
    }

    #[test]
    fn an_on_demand_import_never_probes_the_callable_spaces() {
        // `import okio.Path.foo.*` cannot name a function: a callable has no
        // members to import on demand.
        let r = resolve("package p\nimport okio.Path.foo.*\n", &["okio"]);
        for candidate in &r.candidates {
            assert_ne!(*candidate, Probes::id("okio#Path.foo()"));
            assert_ne!(*candidate, Probes::id("okio#Path.foo!"));
        }
    }

    #[test]
    fn every_probe_is_recorded_and_nothing_else_is() {
        let r = resolve("package p\nimport okio.Buffer\n", &["okio", "okio#Buffer"]);
        assert_eq!(
            r.candidates,
            [Probes::id("okio"), Probes::id("okio#Buffer")],
            "the candidate log must list exactly what was probed",
        );
    }

    #[test]
    fn one_name_declared_in_many_source_sets_is_one_entity() {
        // `expect class Lock` in commonMain and `actual typealias Lock` in
        // jvmMain are the same `okio.Lock`; the identity space carries no
        // source-set dimension, so they merge rather than collide.
        let expect = def(DefKind::Type, "Lock", &["okio"]);
        let actual_alias = def(DefKind::Alias, "Lock", &["okio"]);
        assert!(KtResolver.mergeable(&expect, &actual_alias));
        // Two overloads of one importable name are one node too, because an
        // import names a name and states no arity.
        let a = def(DefKind::Function, "commonWrite", &["okio.internal"]);
        assert!(KtResolver.mergeable(&a, &a.clone()));
        // A different owner is a different entity, whatever the name.
        let elsewhere = def(DefKind::Type, "Lock", &["okio", "Buffer"]);
        assert!(!KtResolver.mergeable(&expect, &elsewhere));
    }

    #[test]
    fn a_reference_kind_this_track_does_not_emit_is_still_answered() {
        let t: HashSet<NodeId> = HashSet::new();
        let r = KtResolver.resolve(
            &(),
            &KtScope,
            &Reference {
                kind: RefKind::Call,
                space: DeclSpace::Value,
                raw_target: "f".to_string(),
                target: RefTarget {
                    root: TargetRoot::Name,
                    segments: vec!["f".to_string()],
                },
                locally_bound: false,
                argc: Some(0),
                enclosing: None,
                span: Span {
                    byte_start: 0,
                    byte_end: 1,
                    line: 1,
                },
            },
            &t,
        );
        assert_eq!(
            r.outcome,
            Outcome::Unresolved(UnresolvedReason::TierTwoLanguage),
        );
    }

    #[test]
    fn the_config_is_empty_because_no_manifest_decides_a_kotlin_identity() {
        assert!(KtResolver.config_digest(&()).is_empty());
        assert!(KtResolver.link_kinds().is_empty());
        assert!(
            KtResolver
                .index_keys(
                    &(),
                    &Fqn::new("okio#Buffer"),
                    &def(DefKind::Type, "Buffer", &["okio"])
                )
                .is_empty()
        );
    }
}
