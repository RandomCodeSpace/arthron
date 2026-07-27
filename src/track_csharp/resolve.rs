//! The one place a C# [`crate::Outcome`] is produced. Never drops.
//!
//! # The gate is an import-resolution rate
//!
//! C# is a **tier-2** language here: definitions, structure and imports, and
//! no verified call edges. So the references this resolver classifies are
//! exactly the `using` directives the extractor emits, and the rate the gate
//! defends is the share of them that name something this repository declares.
//! Every one ends `Resolved`, `External`, or `Unresolved` with a reason —
//! there is no way to express "dropped".
//!
//! # The import model
//!
//! A `using` names an absolute name, so resolution is a probe rather than a
//! search — which is why [`CsScope`] carries only the clause each reference
//! belongs to. The rules, in order:
//!
//! 1. **A plain `using A.B;` names a namespace.** Probe the namespace key
//!    `A.B`. A hit is [`crate::Outcome::Resolved`]. A namespace is declared by
//!    the files that write `namespace A.B;`, and by every file that writes a
//!    namespace *under* it — `namespace A.B.C;` declares `A.B` too.
//! 2. **A namespace this repository does not declare is declared by another
//!    assembly** ⇒ [`crate::Outcome::External`], named by the root segment.
//!    There is no third possibility in a corpus that compiles: a `using` of a
//!    namespace no referenced assembly supplies is a compile error (CS0246),
//!    so a name absent from a complete in-repository namespace set is
//!    somebody else's.
//! 3. **`using static A.B.C;` names a type.** Probe the type key for each
//!    namespace/type split, longest namespace first, so a nested type is
//!    reached as well as a top-level one: `A.B.C.D` probes `A.B.C#D`, then
//!    `A.B#C+D`, then `A#B+C+D`, then `#A+B+C+D`.
//! 4. **`using X = A.B.C;` may name either.** C# lets an alias bind a
//!    namespace as readily as a type and nothing at the site says which, so
//!    rule 3's probes run first and rule 1's on the whole name after.
//! 5. **A type-shaped miss whose immediate container this repository declares
//!    is our own miss** ⇒ [`crate::UnresolvedReason::NoMatchingDefinition`],
//!    the bucket reserved for meaning *our* bug. Otherwise ⇒
//!    [`crate::Outcome::External`].
//!
//! # Why rule 2 is a probe and not a prefix claim
//!
//! PHP asks whether a declared PSR-4 prefix *claims* a missing name, and
//! answers [`crate::UnresolvedReason::ModuleNotFound`] when one does. C# has
//! no such construct, and this corpus proves it rather than merely asserting
//! it: `src/Serilog/Util/TimeProvider.cs` declares `namespace System;` — a
//! polyfill under `#if !NET8_0_OR_GREATER` — while 33 of the corpus's
//! `using` directives name `System.*` namespaces that only the BCL supplies.
//! A rule reading "this repository declares `System`, so `System.Diagnostics`
//! should be here" would call all 33 of them in-repository misses. C#
//! namespaces are *open*: any assembly may add to any namespace, and none
//! owns a prefix. So the question a `using` asks is answered by an exact
//! probe, and nothing else.
//!
//! # What that costs, and why the answer is not laundering
//!
//! `External` sits outside **both** terms of the resolution rate, so a rule
//! that widens it raises the rate without linking anything. Two things keep
//! rule 2 honest:
//!
//! - The set it is measured against is **complete and measured**, not
//!   remembered. Ruby cannot tell its standard library from a load root it
//!   got wrong, so `require 'time'` is
//!   [`crate::UnresolvedReason::UnknownPackage`] there and counts against the
//!   rate. C#'s in-repository namespace set is every `namespace` declaration
//!   in every file the walk read — there is no list written from memory
//!   anywhere in this track.
//! - The count itself is gated. `external` is a baseline field, and any drift
//!   in it fails the gate and has to be re-based deliberately. A change that
//!   quietly moved references from `Resolved` to `External` would raise the
//!   rate and fail the build in the same breath.
//!
//! What neither of those catches is a name this track spelled wrong *before*
//! the baseline was recorded: a namespace this repository declares, probed
//! under a key the extractor never minted, is `External` on the first run and
//! on every run after it. So the burden sits on the extractor building the
//! namespace set exactly, and on fixtures rather than on the rate — which is
//! why `namespace A { namespace B { … } }` is composed into `A.B` rather than
//! read off one field, why `namespace A.B.C;` implies `A.B` and `A` through
//! [`crate::track_csharp::lang::implied_namespaces`], and why both have a
//! fixture in `tests/csharp_extract.rs` and one in `tests/csharp_resolve.rs`.
//!
//! # Known limits, recorded rather than left to be rediscovered
//!
//! - **A nested type under rule 5.** `using static A.B.C.D` where `A.B` is
//!   ours, `C` a type in it and `D` nested in `C` reads its immediate
//!   container as `A.B.C`, which is not a namespace, so a miss falls to
//!   `External` rather than to `NoMatchingDefinition`. The corpus contains no
//!   `using static` of a nested type; a corpus that has one is what would
//!   earn the finer rule.
//! - **A one-segment type-shaped target** — `using static Math;` — reads its
//!   container as the global namespace, which every C# file has and every C#
//!   scan therefore holds (see [`crate::track_csharp::extract`]), so a miss is
//!   `NoMatchingDefinition` rather than `External`. That is the conservative
//!   direction on purpose: where a rule cannot tell the two apart, the answer
//!   that counts *against* the rate is the one that cannot launder a miss.
//!
//!   It is also the *same* direction in every repository. It was not always:
//!   while the global namespace was minted only by a file that declared no
//!   namespace of its own, this line read `External` in a repository where
//!   every file declared one and `NoMatchingDefinition` in the same
//!   repository with a single `GlobalUsings.cs` added beside it. One source
//!   line classified two ways by an unrelated file is not a rule, and the
//!   global namespace's existence is a fact about C# rather than about what
//!   some other file happened to write. The corpus contains no such
//!   directive; `tests/csharp_resolve.rs` is what holds this.
//! - **Assembly boundaries are not modelled.** A namespace is one identity
//!   across the whole repository, which is what C# says — a namespace is not
//!   owned by an assembly. Whether the *project* a file belongs to references
//!   the project that declares a namespace is a compile-time constraint, and
//!   in a corpus that compiles it can never change an answer: a `using` of a
//!   namespace the project cannot see does not build.
//! - **`InternalsVisibleTo` is not read.** `src/Serilog/Properties/AssemblyInfo.cs`
//!   grants the test project access to the library's `internal` members, which
//!   is what makes the test project's references legal. Nothing here is
//!   refused on visibility grounds, so nothing needs the grant; recovering the
//!   target out of the attribute's string literal is a framework-rule shape
//!   and never the core resolver's.
//! - **`LocalBinding` does not apply.** Tier 2 emits no expression-level
//!   reference, so no C# reference can name a parameter, a local or a
//!   receiver. The bucket stays empty, and the baseline records it as zero —
//!   which makes this rate un-gameable by the one reclassification the rate's
//!   own definition permits.

use std::collections::HashMap;
use std::path::Path;

use crate::lang::{
    Extractor, FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe,
};
use crate::model::{DefKind, Definition, Fqn, NodeId, RefKind, Reference, node_id};
use crate::track_csharp::extract::{CsExtractor, CsHeader, ImportForm};
use crate::track_csharp::lang::{CsLang, arity_name, member_fqn, namespace_fqn, type_fqn};
use crate::{Outcome, UnresolvedReason};

/// C#'s project configuration: nothing.
///
/// Not an oversight and not a placeholder. C# states a type's namespace in
/// the source and nowhere else, and a `using` names an absolute name, so no
/// manifest mediates between a name and where it lives — which is the whole
/// of what Go's `go.mod`, PHP's `composer.json` and Rust's `Cargo.toml` do
/// for their tracks. A `.csproj` decides which *assemblies* a compilation
/// sees and which `FEATURE_*` symbols are defined; neither changes what a
/// name means, and this track resolves neither (see the module docs for why
/// assembly boundaries cannot change an answer on a corpus that compiles, and
/// [`crate::track_csharp::extract`] for why both arms of a `#if` are read).
///
/// Its digest is therefore empty, which the [`Resolver::config_digest`]
/// contract names explicitly: a language with no project manifest is never
/// invalidated by one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CsProject;

/// One file's view of what its own imports mean: the clause each reference
/// belongs to, keyed by the span the two share.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CsScope {
    imports: HashMap<(u32, u32), ImportForm>,
}

/// The C# resolver. Stateless: everything it reads is in the scope or the
/// probe.
pub struct CsResolver;

/// Every probe a resolution made, in read order, hits and misses alike.
struct Probes<'a> {
    table: &'a dyn SymbolProbe,
    seen: Vec<NodeId>,
}

impl Probes<'_> {
    fn hit(&mut self, fqn: &str) -> bool {
        let id = Probes::id(fqn);
        self.seen.push(id);
        self.table.probe(&id).is_some()
    }

    fn id(fqn: &str) -> NodeId {
        node_id(CsLang::DOMAIN, fqn)
    }
}

/// The name an external node is filed under: the root namespace segment.
///
/// The coarsest unit C#'s own name resolution keys on, and the only one this
/// build can name without guessing. A package name does not give its
/// namespace — `xunit` supplies `Xunit`, `Microsoft.NET.Test.Sdk` supplies
/// nothing a source file names — so reading `<PackageReference>` out of the
/// `.csproj` would buy a guess rather than a fact, which is the lesson PHP
/// wrote down for `guzzlehttp/promises`.
fn external_package(segments: &[String]) -> String {
    segments
        .first()
        .cloned()
        .unwrap_or_else(|| "csharp:global".to_string())
}

impl CsResolver {
    /// Rule 3: every namespace/type split of a dotted name, longest namespace
    /// first, so a nested type is reached as well as a top-level one.
    fn type_candidates(segments: &[String]) -> Vec<String> {
        (0..segments.len())
            .rev()
            .map(|at| type_fqn(&segments[..at].join("."), &segments[at..]))
            .collect()
    }

    /// Rule 5: a type-shaped name that named nothing.
    fn type_miss(segments: &[String], p: &mut Probes) -> Outcome<NodeId, String> {
        let Some((_, container)) = segments.split_last() else {
            return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition);
        };
        let namespace = namespace_fqn(&container.join("."));
        if p.hit(&namespace) {
            // The container is a namespace this repository declares, and the
            // name is not in it. That is the one case the reason reserved for
            // "our own bug" describes.
            return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition);
        }
        Outcome::External(external_package(segments))
    }

    /// Rules 1 and 2: a plain `using`.
    fn namespace_import(segments: &[String], p: &mut Probes) -> Outcome<NodeId, String> {
        let key = namespace_fqn(&segments.join("."));
        if p.hit(&key) {
            return Outcome::Resolved(Probes::id(&key));
        }
        Outcome::External(external_package(segments))
    }

    /// Rule 3, then rule 5: `using static`.
    fn static_import(segments: &[String], p: &mut Probes) -> Outcome<NodeId, String> {
        for key in Self::type_candidates(segments) {
            if p.hit(&key) {
                return Outcome::Resolved(Probes::id(&key));
            }
        }
        Self::type_miss(segments, p)
    }

    /// Rules 3, 4 and 5: an alias, which may name either table.
    fn alias_import(segments: &[String], p: &mut Probes) -> Outcome<NodeId, String> {
        for key in Self::type_candidates(segments) {
            if p.hit(&key) {
                return Outcome::Resolved(Probes::id(&key));
            }
        }
        let whole = namespace_fqn(&segments.join("."));
        if p.hit(&whole) {
            return Outcome::Resolved(Probes::id(&whole));
        }
        Self::type_miss(segments, p)
    }
}

impl Resolver<CsLang> for CsResolver {
    /// Phase 0 reads nothing. See [`CsProject`].
    fn config(&self, _root: &Path, _files: &FileIndex) -> Result<CsProject, LayoutError> {
        Ok(CsProject)
    }

    /// Empty: no manifest decides any identity here, so no manifest can
    /// invalidate a store.
    fn config_digest(&self, _cfg: &CsProject) -> Vec<u8> {
        Vec::new()
    }

    /// `None`: a C# identity is decided by the `namespace` the file itself
    /// declares, so both phases build the same names from the same bytes and
    /// there is nothing to learn from the store. A file may declare several
    /// namespaces anyway, and this asks for one.
    fn declared_container(&self, _cfg: &CsProject, _header: &CsHeader) -> Option<(String, String)> {
        None
    }

    /// Nothing to learn, for the reason [`Resolver::declared_container`]
    /// gives.
    fn learn_containers(&self, _cfg: &mut CsProject, _names: &HashMap<String, String>) {}

    /// Every file the walk reached. There is no nested-manifest fence: a
    /// repository holds one `.csproj` per project by design, and every one of
    /// them is this repository's own code. What is genuinely not ours is the
    /// build's output, and that is pruned from the walk by
    /// [`CsLang::skip_dirs`] rather than filtered out of it.
    fn owns_file(&self, _cfg: &CsProject, _rel_path: &str) -> bool {
        true
    }

    fn def_fqn(
        &self,
        _cfg: &CsProject,
        _header: &CsHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        // A namespace names itself. Everything else carries its namespace as
        // `owner[0]` and its enclosing types after it, so a file that
        // declares several namespaces still files each declaration under the
        // one it sits in.
        if def.kind == DefKind::Module {
            return Some(Fqn::new(namespace_fqn(&def.name)));
        }
        let namespace = owner.first()?.as_str();
        let outer = &owner[1..];
        if def.kind == DefKind::Type {
            let arity = def.params.as_ref().map_or(0, |p| p.count as usize);
            let mut path = outer.to_vec();
            path.push(arity_name(&def.name, arity));
            return Some(Fqn::new(type_fqn(namespace, &path)));
        }
        // A member. C# has no member outside a type, so an owner that names
        // none is not a shape this extractor produces and not one a name is
        // invented for.
        if outer.is_empty() {
            return None;
        }
        let owning_type = type_fqn(namespace, outer);
        let key = match def.kind {
            // C# overloads on the parameter list, so the key carries it.
            DefKind::Method | DefKind::Constructor => {
                let types = def
                    .params
                    .as_ref()
                    .map(|p| p.types.join(","))
                    .unwrap_or_default();
                format!("{}({types})", def.name)
            }
            // A field, a property, an event, an indexer or an enum member.
            // C# keeps one name table per type for all of them — a class may
            // not hold a field `X` and a property `X` — so the name is the
            // whole key.
            _ => def.name.clone(),
        };
        Some(Fqn::new(member_fqn(&owning_type, &key)))
    }

    /// Empty: C# reaches every definition by its FQN alone. There is no
    /// export alias — `using X = A.B;` binds `X` in one file and is nameable
    /// from nowhere else — and an overload set is separated by the FQN rather
    /// than indexed beside it.
    fn index_keys(&self, _cfg: &CsProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        Vec::new()
    }

    /// Two C# declarations that agree on kind, name, owner and parameter
    /// shape are one entity, and the language writes them apart routinely:
    ///
    /// - **`partial`.** A type written across several files is one type, and
    ///   so is a method whose declaration and implementation are split.
    /// - **`#if`.** The extractor reads both arms of a conditional, so a
    ///   member declared identically under `#if` and `#else` arrives twice.
    ///
    /// Anything else sharing an identity is a genuine collision: a class and
    /// an interface of one name in one namespace never co-compile, and
    /// merging them would let one declaration's sites stand in for another's.
    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        a.kind == b.kind && a.name == b.name && a.owner == b.owner && a.params == b.params
    }

    fn scope(
        &self,
        _cfg: &CsProject,
        file: &FileFacts<CsLang>,
        _probe: &dyn SymbolProbe,
    ) -> CsScope {
        CsScope {
            imports: file
                .header
                .imports
                .iter()
                .map(|i| ((i.span.byte_start, i.span.byte_end), i.form.clone()))
                .collect(),
        }
    }

    /// Empty. Tier 2 emits no inheritance reference, so there is no supertype
    /// relation to derive and no member lookup that would walk one.
    fn link_kinds(&self) -> &'static [RefKind] {
        &[]
    }

    fn resolve(
        &self,
        _cfg: &CsProject,
        scope: &CsScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let mut p = Probes {
            table: probe,
            seen: Vec::new(),
        };
        let outcome = match (
            r.kind,
            scope.imports.get(&(r.span.byte_start, r.span.byte_end)),
        ) {
            (RefKind::Import, Some(ImportForm::Namespace(segments))) => {
                Self::namespace_import(segments, &mut p)
            }
            (RefKind::Import, Some(ImportForm::Static(segments))) => {
                Self::static_import(segments, &mut p)
            }
            (RefKind::Import, Some(ImportForm::Alias { target, .. })) => {
                Self::alias_import(target, &mut p)
            }
            // Both halves are structurally unreachable: this track's
            // extractor emits one reference kind, and it emits a clause and
            // its reference together. Kept because `resolve` is total over
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

/// The C# track's scan entry point, reading every `.cs` the walk finds.
pub fn scan_csharp(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_csharp_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_csharp`] under a repository's include/exclude globs. What
/// [`crate::track_csharp::TRACK`] holds.
pub fn scan_csharp_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<CsLang>(root, db, &CsExtractor, &CsResolver, filter)
}

/// C#'s `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(CsLang::LANG, crate::model::Lang::CSharp));
    assert!(matches!(CsLang::DOMAIN, crate::model::Domain::CSharp));
};

/// The extractor's `Extractor` impl is what the driver runs;
/// [`crate::track_csharp::extract::extract`] is what the fixtures call.
/// Naming both keeps the trait object honest.
const _: fn() = || {
    fn assert_extractor<T: Extractor<CsLang>>() {}
    assert_extractor::<CsExtractor>();
};
