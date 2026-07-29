//! The one place a Scala [`Outcome`] is produced. Never drops.
//!
//! Scala is a **tier-2** language here: the extractor emits definitions,
//! structure and import references, and nothing else. So this resolver
//! answers exactly one question — *what does this path name?* — and its rate
//! is an **import-resolution rate**, not a call-graph one. There is no call
//! site to dispatch and no receiver whose type would have to be inferred,
//! which is why the two reasons that dominate every tier-1 track,
//! [`UnresolvedReason::NeedsReceiverType`] and
//! [`UnresolvedReason::NeedsTypeInference`], are unreachable here rather than
//! small. [`UnresolvedReason::LocalBinding`] is unreachable for the same
//! reason: it is the reason a reference to a *local* carries, and tier 2
//! emits no expression-level reference for a block to bind.
//!
//! # The path model, as Scala states it
//!
//! A Scala import path is looked up **innermost scope first**. The bases this
//! resolver tries, in order, are:
//!
//! 1. the container the site sits in — the enclosing `object`s and the
//!    enclosing packages, innermost first;
//! 2. each enclosing package prefix, out to the root.
//!
//! The first base under which the path's **first segment** binds is the one
//! the whole path is resolved under, and whatever that walk yields — a node
//! or a miss — is the answer. That is Scala's own rule and not a heuristic:
//! an inner binding shadows an outer one, and a first segment that binds
//! settles the lookup even when the rest of the path then fails.
//!
//! Below a base, each segment is probed as a **container** first
//! (`base.segment`) and as a **member** second (`base#segment`), which is the
//! order the two namespaces are searched in, and after the first member every
//! further segment is a member of it.
//!
//! `_root_.a.b` skips step 1 entirely, which is the whole point of writing
//! it.
//!
//! When no scope binds the first segment, one last rule applies: a **top-level
//! import earlier in the same file** is in scope for the rest of it, so
//! `import upickletest.TestUtil` followed by `import TestUtil._` names
//! `upickletest.TestUtil`. The bound path is substituted for the first
//! segment and the walk runs once more; it is never substituted twice, so no
//! chain of bindings can loop. Scala ranks an import below a name a package
//! clause makes available, which is why this is tried last and not first.
//!
//! A *wildcard* import binds no name here. `import p._` followed by `import
//! q._`, where `q` is a member package of `p`, is real Scala and misses: the
//! set a wildcard forwards is a fact about `p` rather than about this file,
//! and probing `p.q` on the strength of an unrelated wildcard is a guess
//! about which of several wildcards in scope supplied the name.
//!
//! # Why nothing here is `External`, and what that costs
//!
//! `External` sits outside **both** terms of the resolution rate, so widening
//! it is the cheapest way there is to raise a rate with nothing linked. Scala
//! offers two tempting widenings and this track takes neither:
//!
//! - **The platform roots.** `import java.nio.ByteBuffer` and `import
//!   scala.collection.mutable` name libraries every Scala compile has on its
//!   classpath without any manifest line — the same argument
//!   [`crate::track_rust`] makes for its five sysroot crates. The difference
//!   is that Rust's list is closed by the toolchain and Scala's is not: `java`
//!   is the JDK on the JVM, a partial emulation on Scala.js and Scala Native,
//!   and the measured corpus cross-builds all three. A root set written from
//!   memory rather than measured is exactly how a rate gets widened without
//!   anything being linked.
//! - **The build's declared dependencies.** `build.mill` names Maven
//!   coordinates — `com.lihaoyi::utest`, `org.json4s::json4s-ast` — and a
//!   coordinate does not state the package prefix its artifact ships.
//!   `com.lihaoyi::utest` ships `utest`; nothing in the build says so.
//!   Deriving one from the other is a guess, and a guess in the column that
//!   leaves the denominator is the worst place to put one.
//!
//! So every path that leaves this repository is
//! [`UnresolvedReason::UnknownPackage`] and counts **against** the rate. The
//! `external` bucket is zero and the gate pins it there, which makes this
//! track's rate un-gameable by the one reclassification the rate's own
//! definition permits.
//!
//! # What the rate cannot reach, recorded rather than left to be found
//!
//! - **An inherited member named by an import.** `import p.O.Thing`, where
//!   `O` is an object that inherits `Thing` from a trait, misses as
//!   [`UnresolvedReason::NoMatchingDefinition`]. Placing it needs the
//!   supertype relation, which is built from *type* references — and a type
//!   reference is precisely what tier 2 does not emit. So this track declares
//!   no [`crate::lang::Resolver::link_kinds`], runs no supertype phase, and
//!   the reason that would be truer here,
//!   [`UnresolvedReason::UnindexedSupertype`], is one this build has no fact
//!   to justify. Recorded as a shortfall rather than papered over: that
//!   reason's own definition requires a supertype set to have been searched,
//!   and none was.
//! - **A path rooted at a term.** `import c.universe._` and `import
//!   quotes.reflect.*` start at a macro context or a `given` — a value whose
//!   *type* names the container. The first segment binds nothing in any
//!   package or object, so the path is `UnknownPackage`, which understates
//!   what a reader can see: 45 of that reason's 305 rows in the measured
//!   corpus name no package at all. Mislabelled in the conservative
//!   direction — every one of them counts *against* the rate — and
//!   distinguishing them needs the declared type of a local, which is
//!   type-directed resolution, which is the tier.
//! - **A class's own members, from an import written inside it.** `class C {
//!   object H; import H.x }` is real Scala: the members of an enclosing
//!   template are in scope. The chain a site carries stops at the first
//!   non-container, so only enclosing *objects* and packages are offered.
//!   Widening it means treating a class as a container in a resolver that
//!   deliberately keeps the two namespaces apart, and nothing in the measured
//!   corpus writes the shape.
//! - **Implicit resolution.** 627 `implicit`/`given` definitions sit behind
//!   the corpus's imports and none of them is a reference site: a `given` is
//!   selected by type at a use this track never reads.
//! - **A wildcard's forwarded set.** `import p._` resolves to `p`, which is
//!   what the site names and all this scan verified. The names it forwards
//!   are a fact about `p` rather than about this file and are never
//!   enumerated — so nothing here can fire
//!   [`UnresolvedReason::WildcardImport`], because at tier 2 no later
//!   reference depends on that set and there is no site at which the reason
//!   could be the answer. A tier-1 Scala track is what would earn it.
//! - **The build configuration.** 26 fully-qualified names in the measured
//!   corpus are each written in two or three source roots, one per platform
//!   or Scala version — nine `object`s, six types and eleven members. The
//!   graph holds the union, `mergeable` says so, and a path naming one of
//!   them resolves to an identity several files declare. The nine `object`s
//!   are countable only because [`ScalaResolver::stores_as_package`] files a
//!   container that is a *term* as a definition; a package node several
//!   files declare is not a collision and never could be.

use std::collections::HashMap;
use std::path::Path;

use crate::lang::{FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe};
use crate::model::{
    DeclSpace, DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, RefTarget, Reference,
    TargetRoot, node_id,
};
use crate::track_scala::extract::{ImportBinding, ScalaExtractor, ScalaHeader};
use crate::track_scala::lang::{
    ROOT, ScalaLang, ScalaProject, container_fqn, is_container, opens_scope, push_segment, unmark,
};
use crate::{Outcome, UnresolvedReason};

/// What one file's references are resolved against.
///
/// One fact and no more: the package the file declares, which is the outer
/// half of every scope a path is looked up in. The inner half — the enclosing
/// objects — travels with each reference, because it differs per site.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScalaScope {
    /// The file's package as marked container segments, outermost first.
    package: Vec<String>,
    /// Every name a top-level import binds, in source order.
    bindings: Vec<ImportBinding>,
}

/// Where a path landed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placed {
    /// It named this FQN inside the repository.
    Node(String),
    /// It could not be placed, for this reason.
    Missing(UnresolvedReason),
}

fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// The container chain a reference site sits in, innermost last.
///
/// Read off the marked prefix of the site's encloser, which the extractor
/// writes from the same ancestor walk that builds every owner chain — so the
/// scope a path is looked up in and the identity its definitions were filed
/// under cannot disagree. A file-scope import has no encloser, and its chain
/// is the file's package.
fn site_chain(r: &Reference, scope: &ScalaScope) -> Vec<String> {
    match &r.enclosing {
        Some(encloser) => encloser
            .path
            .iter()
            .take_while(|s| is_container(s))
            .cloned()
            .collect(),
        None => scope.package.clone(),
    }
}

/// Every base a path's first segment is looked up in, innermost first.
///
/// A prefix of the chain is a scope only when the container it ends at opens
/// one. `package a.b` opens exactly one — `a.b` — because Scala does not put
/// the members of `a` in scope for a qualified clause; `package a` followed
/// by `package b` opens two. The root package always closes the list.
fn bases(chain: &[String]) -> Vec<String> {
    (0..=chain.len())
        .rev()
        .filter(|cut| *cut == 0 || opens_scope(&chain[*cut - 1]))
        .map(|cut| container_fqn(&chain[..cut]))
        .collect()
}

/// Probe one identity, recording it whether it hits or misses.
fn probed(probe: &dyn SymbolProbe, candidates: &mut Vec<NodeId>, fqn: &str) -> bool {
    let id = node_id(Domain::Scala, fqn);
    if !candidates.contains(&id) {
        candidates.push(id);
    }
    probe.probe(&id).is_some()
}

/// Walk a path's segments below one base.
///
/// `Err` carries the reason **and the index the walk stopped at**, because
/// index 0 means something different from every other index: the first
/// segment did not bind in this scope, so this is not the scope Scala's
/// lookup would have chosen, and the caller must try the next one out rather
/// than report a miss.
fn walk(
    base: &str,
    segments: &[String],
    probe: &dyn SymbolProbe,
    candidates: &mut Vec<NodeId>,
) -> Result<String, (UnresolvedReason, usize)> {
    let mut current = base.to_string();
    let mut members = false;
    for (at, segment) in segments.iter().enumerate() {
        if !members {
            let container = format!("{current}.{segment}");
            if probed(probe, candidates, &container) {
                current = container;
                continue;
            }
            let member = format!("{current}#{segment}");
            if probed(probe, candidates, &member) {
                current = member;
                members = true;
                continue;
            }
        } else {
            let member = format!("{current}.{segment}");
            if probed(probe, candidates, &member) {
                current = member;
                continue;
            }
        }
        // The container the segment should have been in was itself placed —
        // the walk got here — so a miss on the *last* segment is a complete
        // lookup that found nothing, and a miss before it is a path that
        // stopped short of the container it needed.
        let reason = if at + 1 == segments.len() {
            UnresolvedReason::NoMatchingDefinition
        } else {
            UnresolvedReason::ModuleNotFound
        };
        return Err((reason, at));
    }
    Ok(current)
}

/// Try one path in every scope the site sits in, innermost first.
///
/// `Missing(UnknownPackage)` means, and only ever means, that **no scope
/// bound the first segment** — which is what lets the caller tell "this path
/// leaves the repository" from "this path named a container and then failed
/// inside it".
fn place_path(
    chain: &[String],
    segments: &[String],
    probe: &dyn SymbolProbe,
    candidates: &mut Vec<NodeId>,
) -> Placed {
    for base in bases(chain) {
        match walk(&base, segments, probe, candidates) {
            Ok(fqn) => return Placed::Node(fqn),
            Err((_, 0)) => continue,
            Err((reason, _)) => return Placed::Missing(reason),
        }
    }
    Placed::Missing(UnresolvedReason::UnknownPackage)
}

/// Resolve one import path against the graph, innermost scope first.
fn place(
    scope: &ScalaScope,
    chain: &[String],
    target: &RefTarget,
    at: u32,
    probe: &dyn SymbolProbe,
) -> (Placed, Vec<NodeId>) {
    let mut candidates: Vec<NodeId> = Vec::new();
    if !matches!(target.root, TargetRoot::Name) {
        // Unreachable while the extractor emits import paths alone: no
        // reference here has an expression, a `this` or a `super` at its
        // root. Answered rather than asserted, because the resolver never
        // drops.
        return (
            Placed::Missing(UnresolvedReason::NeedsExpressionType),
            candidates,
        );
    }
    let segments = &target.segments;
    let Some(first) = segments.first() else {
        return (
            Placed::Missing(UnresolvedReason::ModuleNotFound),
            candidates,
        );
    };
    // `_root_` is Scala's own way of saying "skip every enclosing scope".
    if first == ROOT {
        let rest = &segments[1..];
        if rest.is_empty() {
            return (
                Placed::Missing(UnresolvedReason::ModuleNotFound),
                candidates,
            );
        }
        return match walk(ROOT, rest, probe, &mut candidates) {
            Ok(fqn) => (Placed::Node(fqn), candidates),
            Err((reason, _)) => (Placed::Missing(reason), candidates),
        };
    }
    let placed = place_path(chain, segments, probe, &mut candidates);
    if placed != Placed::Missing(UnresolvedReason::UnknownPackage) {
        return (placed, candidates);
    }
    // No scope bound the first segment. A top-level import earlier in this
    // file may have: substitute the path it bound and walk once more. Once,
    // and never again — a second substitution would be a chain this build has
    // no reason to believe in and a loop it would then have to detect.
    if let Some(binding) = continuation(scope, first, at) {
        let mut combined = binding.segments.clone();
        combined.extend_from_slice(&segments[1..]);
        let retried = place_path(chain, &combined, probe, &mut candidates);
        if retried != Placed::Missing(UnresolvedReason::UnknownPackage) {
            return (retried, candidates);
        }
    }
    // The first segment bound in no enclosing scope, in no earlier import,
    // and in no package this repository declares, so the path leaves the
    // repository at its root. Saying that is more useful than saying a name
    // is absent — and it is `Unresolved`, not `External`: see the module docs
    // for why this track mints no external node at all.
    (
        Placed::Missing(UnresolvedReason::UnknownPackage),
        candidates,
    )
}

/// The nearest top-level import above `at` that binds `name`.
///
/// The *nearest*, because a later import of one name shadows an earlier one,
/// and above `at`, because an import is in scope from where it is written and
/// not before.
fn continuation<'s>(scope: &'s ScalaScope, name: &str, at: u32) -> Option<&'s ImportBinding> {
    scope
        .bindings
        .iter()
        .rfind(|b| b.name == name && b.byte_start < at)
}

/// Scala's resolver. Stateless: everything it reads is in the scope, the
/// reference, or the probe.
pub struct ScalaResolver;

impl Resolver<ScalaLang> for ScalaResolver {
    fn config(&self, _root: &Path, _files: &FileIndex) -> Result<ScalaProject, LayoutError> {
        // Phase 0 reads nothing. A Scala file states its own package, so
        // every fact a path is resolved against is already in the tree the
        // walk read — see [`ScalaProject`] for why that is a property of the
        // language and not a gap.
        Ok(ScalaProject)
    }

    fn config_digest(&self, _cfg: &ScalaProject) -> Vec<u8> {
        Vec::new()
    }

    fn declared_container(
        &self,
        _cfg: &ScalaProject,
        _header: &ScalaHeader,
    ) -> Option<(String, String)> {
        // A Scala file names no container for anybody else: `package
        // upickle.core` is written in twenty-seven files and reopens one
        // package, and the package's identity is the name itself.
        None
    }

    fn learn_containers(&self, _cfg: &mut ScalaProject, _names: &HashMap<String, String>) {
        // Nothing a Scala reference binds is derived from another file's
        // source, so there is nothing to learn.
    }

    fn owns_file(&self, _cfg: &ScalaProject, _rel_path: &str) -> bool {
        // No nested-manifest fence: mill and sbt describe modules of one
        // build, not separate projects the way a nested `go.mod` does.
        true
    }

    fn def_fqn(
        &self,
        _cfg: &ScalaProject,
        _header: &ScalaHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        let mut fqn = String::from(ROOT);
        let mut members = false;
        for segment in owner {
            push_segment(
                &mut fqn,
                &mut members,
                unmark(segment),
                is_container(segment),
            );
        }
        let name = unmark(&def.name);
        if name.is_empty() {
            // The root package: a file that writes no `package` clause still
            // has a container, and it is the one Scala calls `_root_`. An
            // empty name anywhere else is a declaration this build could not
            // read, and is not nameable.
            return owner.is_empty().then(|| Fqn::new(fqn));
        }
        push_segment(&mut fqn, &mut members, name, def.kind == DefKind::Module);
        Some(Fqn::new(fqn))
    }

    fn index_keys(&self, _cfg: &ScalaProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // Every Scala node here is reachable by exactly one identity: the
        // path a `import` would write to it.
        Vec::new()
    }

    fn stores_as_package(&self, def: &Definition) -> bool {
        // Scala files two different things under `DefKind::Module`, and only
        // one of them is a package. `package p` and `package object p` are
        // namespaces: every file under `p` reopens it, and a node several
        // files declare is what a package *is*. `object O` is a term — a
        // single declaration that happens to be a container in the FQN
        // grammar — and two files declaring one are two entities, exactly as
        // two files declaring one `class` are.
        //
        // The extractor already separated them, in `space_of_container`; this
        // is where that distinction reaches the graph. Without it an `object`
        // written once per build configuration would be stored as one package
        // node with several declaration sites, contribute nothing to
        // `fqn_collisions`, and never reach `mergeable` below — the union
        // over build configurations would hold, but nothing would count it.
        def.kind == DefKind::Module && def.space == DeclSpace::Namespace
    }

    fn mergeable(&self, _a: &Definition, _b: &Definition) -> bool {
        // Two *declarations* sharing an FQN are two entities, never one. The
        // measured corpus is built across five Scala versions and three
        // platforms, and 26 fully-qualified names are each written in two or
        // three source roots — `upickle.WebJson` in `src-js`, `src-jvm` and
        // `src-native`, `upickle.core.compat.SortInPlace` in `src-2.12` and
        // `src-2.13+`. Every one of them is real under its own build, the
        // graph holds the union over configurations, and merging them would
        // hide exactly that.
        //
        // A *package* never reaches this question, because
        // `stores_as_package` above already answered `true` for it and being
        // declared by every file in it is what a package is. An `object`
        // does reach it, and the answer is the same `false` a `class` gets.
        false
    }

    fn scope(
        &self,
        _cfg: &ScalaProject,
        file: &FileFacts<ScalaLang>,
        _probe: &dyn SymbolProbe,
    ) -> ScalaScope {
        ScalaScope {
            package: file.header.package.clone(),
            bindings: file.header.bindings.clone(),
        }
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // Tier 2 emits no `Inherit` reference: `class C extends Base` is part
        // of `C`'s structure here and is not resolved, so there is no
        // supertype relation to build and nothing for the driver to run a
        // phase over. What that costs is stated in the module docs.
        &[]
    }

    fn resolve(
        &self,
        _cfg: &ScalaProject,
        scope: &ScalaScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        if r.kind != RefKind::Import {
            // Unreachable while the extractor emits imports alone. Answered
            // rather than asserted: the resolver never drops, and a future
            // reference kind must arrive with a rule rather than a panic.
            return unresolved(UnresolvedReason::TierTwoLanguage);
        }
        let (placed, candidates) = place(
            scope,
            &site_chain(r, scope),
            &r.target,
            r.span.byte_start,
            probe,
        );
        let outcome = match placed {
            Placed::Node(fqn) => Outcome::Resolved(node_id(Domain::Scala, &fqn)),
            Placed::Missing(reason) => Outcome::Unresolved(reason),
        };
        Resolution {
            outcome,
            candidates,
        }
    }
}

/// The Scala track's scan entry point, reading every file the walk finds that
/// Scala owns.
pub fn scan_scala(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_scala_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_scala`] under a repository's include/exclude globs. What
/// [`crate::track_scala::TRACK`] holds.
pub fn scan_scala_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<ScalaLang>(root, db, &ScalaExtractor, &ScalaResolver, filter)
}

/// Scala's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(ScalaLang::LANG, Lang::Scala));
    assert!(matches!(ScalaLang::DOMAIN, Domain::Scala));
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeclSpace, DefFacets, Span};
    use crate::track_scala::extract::extract;
    use crate::track_scala::lang::{mark, mark_qualifier};
    use std::collections::HashSet;

    /// A symbol table holding exactly these FQNs.
    fn table(fqns: &[&str]) -> HashSet<NodeId> {
        fqns.iter().map(|f| node_id(Domain::Scala, f)).collect()
    }

    /// Resolve every import in one file against one table.
    fn outcomes(rel: &str, source: &str, known: &[&str]) -> Vec<(String, Outcome<NodeId, String>)> {
        let facts = extract(rel, source);
        let table = table(known);
        let scope = ScalaResolver.scope(&ScalaProject, &facts, &table);
        facts
            .refs
            .iter()
            .map(|r| {
                (
                    r.raw_target.clone(),
                    ScalaResolver
                        .resolve(&ScalaProject, &scope, r, &table)
                        .outcome,
                )
            })
            .collect()
    }

    /// The FQN one import resolved to, or the reason it did not.
    fn shown(rel: &str, source: &str, known: &[&str]) -> Vec<(String, String)> {
        let ids: std::collections::HashMap<NodeId, &str> = known
            .iter()
            .map(|f| (node_id(Domain::Scala, f), *f))
            .collect();
        outcomes(rel, source, known)
            .into_iter()
            .map(|(raw, outcome)| {
                let shown = match outcome {
                    Outcome::Resolved(id) => ids.get(&id).copied().unwrap_or("?").to_string(),
                    Outcome::External(pkg) => format!("external:{pkg}"),
                    Outcome::Unresolved(reason) => format!("{reason:?}"),
                };
                (raw, shown)
            })
            .collect()
    }

    fn def_of(kind: DefKind, name: &str) -> Definition {
        Definition {
            kind,
            name: name.to_string(),
            owner: Vec::new(),
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

    fn fqn(owner: &[&str], def: &Definition) -> String {
        let owner: Vec<String> = owner.iter().map(|s| (*s).to_string()).collect();
        ScalaResolver
            .def_fqn(
                &ScalaProject,
                &ScalaHeader::default(),
                &owner,
                def,
                &table(&[]),
            )
            .expect("nameable")
            .into_string()
    }

    // -- the FQN grammar --------------------------------------------------

    #[test]
    fn the_root_package_is_named_and_is_not_the_empty_string() {
        let root = Definition {
            facets: DefFacets::SYNTHETIC,
            ..def_of(DefKind::Module, "")
        };
        assert_eq!(fqn(&[], &root), "_root_");
        // An empty name anywhere else is a declaration this build could not
        // read, and naming it would give two of them one identity.
        let nameless = def_of(DefKind::Type, "");
        assert!(
            ScalaResolver
                .def_fqn(
                    &ScalaProject,
                    &ScalaHeader::default(),
                    &[mark("p")],
                    &nameless,
                    &table(&[]),
                )
                .is_none(),
        );
    }

    #[test]
    fn a_package_a_class_and_a_member_each_get_their_separator() {
        assert_eq!(
            fqn(&[".upickle"], &def_of(DefKind::Module, "core")),
            "_root_.upickle.core",
        );
        assert_eq!(
            fqn(&[".upickle", ".core"], &def_of(DefKind::Type, "Visitor")),
            "_root_.upickle.core#Visitor",
        );
        assert_eq!(
            fqn(
                &[".upickle", ".core", "Visitor"],
                &def_of(DefKind::Method, "visitNull")
            ),
            "_root_.upickle.core#Visitor.visitNull",
        );
        // A member of an object is a member of a *container*, so the hash
        // lands one segment later.
        assert_eq!(
            fqn(
                &[".upickle", ".core", ".Ops"],
                &def_of(DefKind::Function, "of")
            ),
            "_root_.upickle.core.Ops#of",
        );
    }

    #[test]
    fn a_companion_pair_under_a_package_never_shares_an_identity() {
        let class = fqn(&[".p"], &def_of(DefKind::Type, "Foo"));
        let object = fqn(&[".p"], &def_of(DefKind::Module, "Foo"));
        assert_eq!(class, "_root_.p#Foo");
        assert_eq!(object, "_root_.p.Foo");
    }

    #[test]
    fn a_companion_pair_under_a_type_does_share_one() {
        // The cost of the one-crossing grammar, recorded rather than left to
        // be discovered: the `#` is spent on `C`, so every segment below it
        // is dotted and the term/type distinction that separated the pair
        // above has nowhere left to be written. The two merge into one node
        // carrying both declaration sites. Both sites are in one file — a
        // companion pair has to be — so this never reaches the cross-build
        // count; the corpus test names all nine such pairs in upickle.
        let class = fqn(&[".p", "C"], &def_of(DefKind::Type, "X"));
        let object = fqn(&[".p", "C"], &def_of(DefKind::Module, "X"));
        assert_eq!(class, "_root_.p#C.X");
        assert_eq!(object, class);
    }

    #[test]
    fn an_object_is_a_definition_and_a_package_is_a_package() {
        // The two share `DefKind::Module`, and only the space tells them
        // apart. Storing an `object` as a package node would give it a
        // record that is exempt from the collision count by design, so a
        // cross-built one would keep both its declaration sites and be
        // counted nowhere.
        let object = def_of(DefKind::Module, "Foo");
        assert_eq!(object.space, DeclSpace::Value);
        assert!(!ScalaResolver.stores_as_package(&object));

        let package = Definition {
            space: DeclSpace::Namespace,
            ..def_of(DefKind::Module, "p")
        };
        assert!(ScalaResolver.stores_as_package(&package));

        // Everything that is not a container answers `false` whatever its
        // space, exactly as the default does.
        for kind in [DefKind::Type, DefKind::Function, DefKind::Const] {
            assert!(!ScalaResolver.stores_as_package(&def_of(kind, "x")));
        }
    }

    #[test]
    fn an_enclosers_marked_name_spells_the_same_identity_as_its_definition() {
        // What `Encloser::as_definition` hands back for an import inside
        // `object O`: a plain definition whose name carries the mark and
        // whose facets are empty. It must name the node the definition phase
        // filed.
        let from_encloser = def_of(DefKind::Module, ".O");
        let from_definition = def_of(DefKind::Module, "O");
        assert_eq!(fqn(&[".p"], &from_encloser), fqn(&[".p"], &from_definition));
        assert_eq!(fqn(&[".p"], &from_encloser), "_root_.p.O");
    }

    // -- the path model ---------------------------------------------------

    #[test]
    fn an_absolute_path_walks_packages_then_members() {
        assert_eq!(
            shown(
                "src/A.scala",
                "package upickle.implicits\nimport upickle.core.Visitor\n",
                &[
                    "_root_.upickle",
                    "_root_.upickle.core",
                    "_root_.upickle.core#Visitor"
                ],
            ),
            [(
                "upickle.core.Visitor".to_string(),
                "_root_.upickle.core#Visitor".to_string()
            )],
        );
    }

    #[test]
    fn a_path_walks_through_an_object() {
        assert_eq!(
            shown(
                "src/A.scala",
                "package p\nimport q.O.Inner\n",
                &["_root_.q", "_root_.q.O", "_root_.q.O#Inner"],
            ),
            [("q.O.Inner".to_string(), "_root_.q.O#Inner".to_string())],
        );
    }

    #[test]
    fn a_wildcard_resolves_to_the_container_it_names() {
        assert_eq!(
            shown(
                "src/A.scala",
                "package p\nimport upickle.core._\n",
                &["_root_.upickle", "_root_.upickle.core"],
            ),
            [(
                "upickle.core._".to_string(),
                "_root_.upickle.core".to_string()
            )],
        );
    }

    #[test]
    fn a_relative_path_is_looked_up_in_the_enclosing_package_first() {
        // `import compat._` inside `package upickle.core` is
        // `upickle.core.compat`, and nothing else in the tree is called
        // `compat`.
        assert_eq!(
            shown(
                "src/A.scala",
                "package upickle.core\nimport compat._\n",
                &[
                    "_root_.upickle",
                    "_root_.upickle.core",
                    "_root_.upickle.core.compat"
                ],
            ),
            [(
                "compat._".to_string(),
                "_root_.upickle.core.compat".to_string()
            )],
        );
    }

    #[test]
    fn a_relative_path_is_looked_up_in_the_enclosing_object_before_the_package() {
        // Scala's own rule: an inner binding shadows an outer one. Both
        // `upickletest.Common.Recursive` and `upickletest.Recursive` exist
        // here, and the inner one wins.
        assert_eq!(
            shown(
                "src/A.scala",
                "package upickletest\nobject Common {\n  import Recursive._\n}\n",
                &[
                    "_root_.upickletest",
                    "_root_.upickletest.Common",
                    "_root_.upickletest.Common.Recursive",
                    "_root_.upickletest.Recursive",
                ],
            ),
            [(
                "Recursive._".to_string(),
                "_root_.upickletest.Common.Recursive".to_string()
            )],
        );
        // With only the package-level one present, the walk falls out to it.
        assert_eq!(
            shown(
                "src/A.scala",
                "package upickletest\nobject Common {\n  import Recursive._\n}\n",
                &[
                    "_root_.upickletest",
                    "_root_.upickletest.Common",
                    "_root_.upickletest.Recursive",
                ],
            ),
            [(
                "Recursive._".to_string(),
                "_root_.upickletest.Recursive".to_string()
            )],
        );
    }

    #[test]
    fn an_import_inside_a_method_body_is_still_looked_up_in_its_container() {
        assert_eq!(
            shown(
                "src/A.scala",
                "package p\nobject O {\n  def go(): Unit = { import Helper._ }\n}\n",
                &["_root_.p", "_root_.p.O", "_root_.p.O.Helper"],
            ),
            [("Helper._".to_string(), "_root_.p.O.Helper".to_string())],
        );
    }

    #[test]
    fn a_qualified_package_clause_does_not_put_its_prefix_in_scope() {
        // The measured corpus's sharpest site: `ujson/argonaut/…` writes
        // `package ujson.argonaut` and imports the *Argonaut library*, while
        // an in-repository package called `ujson.argonaut` sits one hop up.
        // Scala does not put `ujson`'s members in scope for a qualified
        // clause, so `argonaut` leaves the repository — and a resolver that
        // did put them in scope would answer with the wrong container here
        // and mint a confidently wrong edge in a repository where it held a
        // `Json`.
        assert_eq!(
            shown(
                "ujson/argonaut/src/ujson/argonaut/ArgonautJson.scala",
                "package ujson.argonaut\nimport argonaut.Json\n",
                &[
                    "_root_.ujson",
                    "_root_.ujson.argonaut",
                    "_root_.ujson.argonaut#Json",
                ],
            ),
            [("argonaut.Json".to_string(), "UnknownPackage".to_string())],
        );
        // Written as two clauses, the same package *does* put `ujson` in
        // scope, and the same import binds in the repository. Two spellings
        // of one package name, two scope sets: that is Scala's rule, not a
        // heuristic.
        assert_eq!(
            shown(
                "ujson/argonaut/src/ujson/argonaut/ArgonautJson.scala",
                "package ujson\npackage argonaut\nimport argonaut.Json\n",
                &[
                    "_root_.ujson",
                    "_root_.ujson.argonaut",
                    "_root_.ujson.argonaut#Json",
                ],
            ),
            [(
                "argonaut.Json".to_string(),
                "_root_.ujson.argonaut#Json".to_string()
            )],
        );
    }

    #[test]
    fn a_braced_package_clause_is_qualified_the_same_way() {
        assert_eq!(
            bases(&[mark("outer"), mark_qualifier("inner"), mark("deep")]),
            [
                "_root_.outer.inner.deep".to_string(),
                "_root_.outer".to_string(),
                "_root_".to_string(),
            ],
        );
    }

    #[test]
    fn a_root_qualified_path_skips_every_enclosing_scope() {
        // `p.p` exists and would win a relative lookup; `_root_` is how the
        // language says to skip it.
        assert_eq!(
            shown(
                "src/A.scala",
                "package p\nimport _root_.p.Target\nimport p.Target\n",
                &[
                    "_root_.p",
                    "_root_.p.p",
                    "_root_.p.p.Target",
                    "_root_.p.Target"
                ],
            ),
            [
                ("_root_.p.Target".to_string(), "_root_.p.Target".to_string()),
                ("p.Target".to_string(), "_root_.p.p.Target".to_string()),
            ],
        );
    }

    #[test]
    fn a_later_import_may_start_at_a_name_an_earlier_one_bound() {
        // The measured corpus's continuation shape: `import
        // upickletest.TestUtil` at the top of the file, `import TestUtil._`
        // inside a test body below it.
        assert_eq!(
            shown(
                "src/A.scala",
                "package upickletest.example\nimport upickletest.TestUtil\nobject T {\n  import TestUtil._\n}\n",
                &[
                    "_root_.upickletest",
                    "_root_.upickletest.example",
                    "_root_.upickletest.TestUtil",
                ],
            ),
            [
                (
                    "upickletest.TestUtil".to_string(),
                    "_root_.upickletest.TestUtil".to_string()
                ),
                (
                    "TestUtil._".to_string(),
                    "_root_.upickletest.TestUtil".to_string()
                ),
            ],
        );
    }

    #[test]
    fn a_binding_reaches_forward_and_never_back() {
        // An import is in scope from where it is written. A path above it
        // must not see it, or a resolver would answer with a container the
        // compiler does not have there either.
        assert_eq!(
            shown(
                "src/A.scala",
                "package p\nimport TestUtil._\nimport q.TestUtil\n",
                &["_root_.p", "_root_.q", "_root_.q.TestUtil"],
            ),
            [
                ("TestUtil._".to_string(), "UnknownPackage".to_string()),
                ("q.TestUtil".to_string(), "_root_.q.TestUtil".to_string()),
            ],
        );
    }

    #[test]
    fn a_renamed_import_binds_its_new_name_and_a_hidden_one_binds_nothing() {
        assert_eq!(
            shown(
                "src/A.scala",
                "package p\nimport q.{Util => U, Other => _}\nimport U.Inner\nimport Other.Inner\n",
                &[
                    "_root_.p",
                    "_root_.q",
                    "_root_.q.Util",
                    "_root_.q.Util.Inner",
                    "_root_.q.Other"
                ],
            ),
            [
                ("q.Util => U".to_string(), "_root_.q.Util".to_string()),
                ("q.Other => _".to_string(), "_root_.q.Other".to_string()),
                ("U.Inner".to_string(), "_root_.q.Util.Inner".to_string()),
                ("Other.Inner".to_string(), "UnknownPackage".to_string()),
            ],
        );
    }

    #[test]
    fn a_wildcard_binds_no_name_for_a_later_import_to_start_at() {
        // Real Scala — `q` is a member package of `p` and the wildcard does
        // bring it into scope — and a miss on purpose: which of the
        // wildcards in scope supplied the name is a question this build
        // answers by guessing or not at all.
        assert_eq!(
            shown(
                "src/A.scala",
                "package top\nimport p._\nimport q.Thing\n",
                &["_root_.top", "_root_.p", "_root_.p.q", "_root_.p.q#Thing"],
            ),
            [
                ("p._".to_string(), "_root_.p".to_string()),
                ("q.Thing".to_string(), "UnknownPackage".to_string()),
            ],
        );
    }

    #[test]
    fn a_nested_import_binds_nothing_beyond_its_own_block() {
        let facts = extract(
            "src/A.scala",
            "package p\nimport top.Level\nobject O {\n  import q.Nested\n}\n",
        );
        let bound: Vec<&str> = facts
            .header
            .bindings
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(bound, ["Level"], "{:?}", facts.header.bindings);
        // Both are still references — a nested import names a container like
        // any other. Only the *binding* stops at its block.
        assert_eq!(facts.refs.len(), 2);
    }

    // -- the misses -------------------------------------------------------

    #[test]
    fn a_first_segment_that_binds_nowhere_leaves_the_repository() {
        // Scala's platform roots and its build's Maven artifacts alike: the
        // path leaves this repository at its root, and nothing here mints an
        // external node for it.
        assert_eq!(
            shown(
                "src/A.scala",
                "package p\nimport java.nio.ByteBuffer\nimport utest._\n",
                &["_root_.p"],
            ),
            [
                (
                    "java.nio.ByteBuffer".to_string(),
                    "UnknownPackage".to_string()
                ),
                ("utest._".to_string(), "UnknownPackage".to_string()),
            ],
        );
    }

    #[test]
    fn a_complete_container_without_the_name_is_a_missing_definition() {
        assert_eq!(
            shown(
                "src/A.scala",
                "package p\nimport q.Absent\n",
                &["_root_.p", "_root_.q"],
            ),
            [("q.Absent".to_string(), "NoMatchingDefinition".to_string())],
        );
    }

    #[test]
    fn a_path_that_stops_short_of_its_container_is_a_missing_module() {
        assert_eq!(
            shown(
                "src/A.scala",
                "package p\nimport q.absent.Thing\n",
                &["_root_.p", "_root_.q"],
            ),
            [("q.absent.Thing".to_string(), "ModuleNotFound".to_string())],
        );
    }

    #[test]
    fn the_scope_the_first_segment_binds_in_settles_the_whole_path() {
        // `q` binds inside `p`, so the path is resolved there and its failure
        // is reported there — the root's own `q` is never consulted, which is
        // exactly what shadowing means.
        assert_eq!(
            shown(
                "src/A.scala",
                "package p\nimport q.Thing\n",
                &["_root_.p", "_root_.p.q", "_root_.q", "_root_.q#Thing"],
            ),
            [("q.Thing".to_string(), "NoMatchingDefinition".to_string())],
        );
    }

    #[test]
    fn a_non_import_reference_says_so_rather_than_being_dropped() {
        let table = table(&[]);
        let r = Reference {
            kind: RefKind::Call,
            space: DeclSpace::Value,
            raw_target: "f".to_string(),
            target: RefTarget {
                root: TargetRoot::Name,
                segments: vec!["f".to_string()],
            },
            locally_bound: false,
            argc: Some(0),
            arg_types: None,
            enclosing: None,
            span: Span {
                byte_start: 0,
                byte_end: 1,
                line: 1,
            },
        };
        let outcome = ScalaResolver
            .resolve(&ScalaProject, &ScalaScope::default(), &r, &table)
            .outcome;
        assert_eq!(
            outcome,
            Outcome::Unresolved(UnresolvedReason::TierTwoLanguage),
        );
    }

    // -- the candidate index ----------------------------------------------

    #[test]
    fn every_probe_is_recorded_hit_or_miss() {
        let facts = extract("src/A.scala", "package p\nimport q.Thing\n");
        let known = table(&["_root_.p", "_root_.q", "_root_.q#Thing"]);
        let scope = ScalaResolver.scope(&ScalaProject, &facts, &known);
        let res = ScalaResolver.resolve(&ScalaProject, &scope, &facts.refs[0], &known);
        let want: Vec<NodeId> = [
            // innermost scope first: `p.q` as a container, then as a member
            "_root_.p.q",
            "_root_.p#q",
            // then the root, where `q` binds; the next segment is probed
            // as a container before it is probed as a member, because that
            // is the order Scala's own lookup takes.
            "_root_.q",
            "_root_.q.Thing",
            "_root_.q#Thing",
        ]
        .iter()
        .map(|f| node_id(Domain::Scala, f))
        .collect();
        assert_eq!(res.candidates, want);
        assert_eq!(
            res.outcome,
            Outcome::Resolved(node_id(Domain::Scala, "_root_.q#Thing")),
        );
    }

    #[test]
    fn a_candidate_is_recorded_once_however_often_it_is_probed() {
        // `_root_` is every path's last base, and a single-segment package
        // name is probed under it exactly once.
        let facts = extract("src/A.scala", "import a.B\n");
        let known = table(&[]);
        let scope = ScalaResolver.scope(&ScalaProject, &facts, &known);
        let res = ScalaResolver.resolve(&ScalaProject, &scope, &facts.refs[0], &known);
        let mut seen = res.candidates.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), res.candidates.len(), "{:?}", res.candidates);
    }

    // -- the configuration ------------------------------------------------

    #[test]
    fn phase_zero_reads_nothing_and_fingerprints_nothing() {
        let cfg = ScalaResolver
            .config(Path::new("."), &FileIndex { files: Vec::new() })
            .expect("a Scala scan needs no manifest");
        assert_eq!(cfg, ScalaProject);
        assert!(ScalaResolver.config_digest(&cfg).is_empty());
    }

    #[test]
    fn two_declarations_of_one_name_are_never_one_entity() {
        let a = def_of(DefKind::Type, "WebJson");
        let b = def_of(DefKind::Type, "WebJson");
        assert!(!ScalaResolver.mergeable(&a, &b));
    }

    #[test]
    fn no_supertype_phase_runs() {
        assert!(ScalaResolver.link_kinds().is_empty());
    }
}
