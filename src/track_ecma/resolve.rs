//! The EcmaScript resolver: the only place an [`Outcome`] is produced, and the
//! only layer that links. One resolver for two languages, because a `.ts` file
//! importing a `.js` definition has to probe an identity that can exist —
//! [`Domain::EcmaScript`] — and because the linking rules are the same rules.
//! Two rates still come out, because the driver tags every row with `L::LANG`.
//!
//! # The FQN grammar
//!
//! ```text
//! fqn      := module | member
//! module   := path                                  -- never contains '#'
//! member   := path '#' space ':' seg ( '.' seg )*   -- exactly one '#'
//! space    := "value" | "type" | "ns"
//! ```
//!
//! `path` is the repo-relative, `/`-separated module path; `seg` is one
//! identifier. Both escape `%`→`%25`, `#`→`%23` and `:`→`%3A`, and a `seg`
//! additionally escapes `.`→`%2E`, so the single unescaped `#` is positional
//! and a module FQN and a definition FQN can never collide in
//! `hash(domain, fqn)`. The escape matters even though it is rare: ES2022
//! permits `export { x as "my-name" }` with any string, including one holding
//! a `#`, and a hash collision is silent. `:` is escaped in the *path* for a
//! second reason that is not this grammar's: `crate::pipeline` keys an
//! external package under `external:<pkg>` and rests that on no FQN
//! containing a colon. A POSIX file name may hold one, so a repository with a
//! file named `external:npm:foo.js` would otherwise hash that module node and
//! the external package `npm:foo.js` to one identity.
//!
//! Four invariants, each of which the core forces rather than merely prefers:
//!
//! 1. **`#` separates a container from its members; `.` only joins identifiers
//!    *within* one container.** The repository convention, and here it is also
//!    a necessity: a module path contains `.` (every file has an extension),
//!    so `path.name` would give the function `parse` of `src/a.ts` and a file
//!    literally named `src/a.ts.parse` one identity.
//! 2. **A space discriminator is mandatory.** TypeScript permits what Go's
//!    spec forbids: `interface Foo {}` beside `const Foo = 1` is two symbols,
//!    and every `class C {}` emits a Value-space and a Type-space record. With
//!    no space in the FQN the two halves of *every* class collide and one
//!    silently overwrites the other.
//! 3. **No occurrence-order component.** No span, no declaration index, no
//!    arity. ECMAScript has no signature-based dispatch (O1/F1), so an arity
//!    component would mint nodes no reference can name; a span component would
//!    re-key a definition whenever an unrelated edit moved it.
//! 4. **The FQN is a function of `(kind, name, owner)` and the module path —
//!    never of [`crate::model::DefFacets`] or [`crate::model::Span`].** This
//!    one is forced: [`Encloser::as_definition`] zeroes the facets and the
//!    span and hardcodes [`DeclSpace::Value`], so any FQN reading them would
//!    make an edge's source disagree with the node it starts at. It is why
//!    `static` does **not** appear in a member FQN, and why a declaration
//!    space that is not `Value` is carried in the *owner chain* as a reserved
//!    segment ([`SPACE_TAG_TYPE`], [`SPACE_TAG_NS`]) that no identifier can
//!    spell.
//!
//! # Export aliases, and why the reasons stay honest
//!
//! A re-export is an alias: an index key naming a node under another name.
//! [`Resolver::def_alias_targets`] turns the extractor's raw text — a
//! specifier and an imported name — into the identity it forwards to, in the
//! definition phase, so the symbol table carries the forward before any
//! reference probes it. `lookup_in_module` then walks those forwards instead
//! of stopping on the first alias it meets, which is what makes an edge
//! through a barrel land on the definition rather than on the barrel.
//!
//! A bare `export * from './x'` is the same mechanism with the target being a
//! *module*: its name set is ES `GetExportedNames`, a fixed point over the
//! module graph, so the walk re-enters each starred module and looks the name
//! up there. The walk is bounded ([`MAX_ALIAS_HOPS`]) and carries a visited
//! set, because the module graph may legally contain a cycle and a resolver
//! that hangs is worse than one that misses.
//!
//! The reasons stay pinned to what the walk actually established:
//!
//! - [`UnresolvedReason::AmbiguousExport`] — two star targets supplied the
//!   name from different definitions. We enumerated both and they disagree.
//! - [`UnresolvedReason::NoMatchingDefinition`] — every target was
//!   enumerated and none has the name. The table really was complete.
//! - [`UnresolvedReason::WildcardImport`] — a star this build could not
//!   enumerate: a CommonJS spread (C9), or a target outside the repository.
//!   The set is genuinely unknown, which is what the reason has always meant.
//! - [`UnresolvedReason::AliasCycle`] — the walk re-entered a key or ran past
//!   the hop ceiling. It stopped *itself*, so blaming the corpus for a
//!   missing definition would be a lie.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::lang::{Entry, FileFacts, FileIndex, LayoutError, Resolution, Resolver, SymbolProbe};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Domain, Encloser, Fqn, NodeId, RefKind, Reference,
    TargetRoot, node_id,
};
use crate::track_ecma::extract::STAR_EXPORT;
use crate::track_ecma::globals;
use crate::track_ecma::lang::{
    Dialect, EcmaHeader, ImportedName, JsLang, ModuleKind, SPACE_TAG_NS, SPACE_TAG_TYPE,
    SPACE_TAG_VALUE, TsLang,
};
use crate::track_ecma::project::{self, EcmaConfig, UNOWNED_CODE_EXTENSIONS};
use crate::track_ecma::specifier::{self, Spec};
use crate::{Outcome, UnresolvedReason};

/// How far a member lookup walks a class's declared supertypes.
///
/// Bounded because ES `ResolveExport` and the prototype chain are both
/// unbounded in principle and three deep in practice; exceeding it reports
/// [`UnresolvedReason::UnindexedSupertype`] rather than looping.
const MAX_SUPER_HOPS: usize = 4;

/// How far an export-alias walk follows re-exports and star chains.
///
/// A barrel chain in a real monorepo is two or three hops — `vue` stars
/// `runtime-dom` stars `runtime-core` — and this sits well above that. The
/// ceiling exists so a pathological or cyclic module graph terminates with
/// [`UnresolvedReason::AliasCycle`] instead of running forever; the cycle
/// guard catches the common case, and this catches a chain that is merely
/// absurd. Both report the same reason because both mean the same thing: the
/// walk stopped itself, and the table was never enumerated.
const MAX_ALIAS_HOPS: usize = 16;

/// The property every instance member of a class lives on.
///
/// A member FQN segment, and an unambiguous one: ES forbids a static class
/// element named `prototype` (15.7 `ClassElementName` — `static prototype` is
/// an early error), so `C.prototype.m` can only ever mean the instance member
/// `m` of `C`, and `C.m` can only ever mean the static one. It is also
/// literally where the language puts the method, which is why E3, E5 and E6
/// all name it: `class C { m(){} }` and `C.prototype.m = function(){}` are one
/// identity across both eras of the language.
pub const PROTOTYPE: &str = "prototype";

/// The identity prefix a star export forwards to when this build cannot name
/// the module at all — a missing file, a specifier no rule resolves.
///
/// It carries a `:`, so no module path and no FQN this resolver builds can
/// equal it, which is exactly the property that makes it work: the target is
/// never a key [`EcmaConfig::module_paths`] answers for, so
/// [`EcmaResolver::walk_stars`] reads it as a branch it could not enumerate
/// and never as a module to enter.
const UNENUMERABLE_MODULE: &str = "unenumerable-module:";

/// Escape a module path for the FQN grammar.
fn escape_path(path: &str) -> String {
    path.replace('%', "%25")
        .replace('#', "%23")
        .replace(':', "%3A")
}

/// Escape one identifier segment for the FQN grammar.
fn escape_seg(seg: &str) -> String {
    seg.replace('%', "%25")
        .replace('#', "%23")
        .replace('.', "%2E")
        .replace(':', "%3A")
}

/// The wire spelling of a declaration space inside an FQN.
fn space_key(space: DeclSpace) -> &'static str {
    match space {
        DeclSpace::Value => "value",
        DeclSpace::Type => "type",
        DeclSpace::Namespace => "ns",
    }
}

/// Split a reserved space tag off the front of an owner chain.
///
/// The tag is how a declaration space survives [`Encloser::as_definition`],
/// which keeps `owner` and `name` and discards everything else.
pub fn split_space_tag(owner: &[String], fallback: DeclSpace) -> (DeclSpace, &[String]) {
    match owner.first().map(String::as_str) {
        Some(SPACE_TAG_TYPE) => (DeclSpace::Type, &owner[1..]),
        Some(SPACE_TAG_NS) => (DeclSpace::Namespace, &owner[1..]),
        Some(SPACE_TAG_VALUE) => (DeclSpace::Value, &owner[1..]),
        _ => (fallback, owner),
    }
}

/// Split a member-owner chain into the class's own path and whether the
/// member sits on the prototype.
///
/// `["C", "prototype"]` is an instance member of `C`; `["C"]` is a static one.
/// The distinction cannot live in [`crate::model::DefFacets`] — invariant 4 —
/// so it lives here, where `Encloser::as_definition` preserves it.
pub fn split_prototype(owner: &[String]) -> (&[String], bool) {
    match owner.last().map(String::as_str) {
        Some(PROTOTYPE) if owner.len() >= 2 => (&owner[..owner.len() - 1], true),
        _ => (owner, false),
    }
}

/// `path#space:a.b.c` for a member path already split into segments.
pub fn member_fqn(module: &str, space: DeclSpace, segments: &[String]) -> String {
    let joined: Vec<String> = segments.iter().map(|s| escape_seg(s)).collect();
    format!(
        "{}#{}:{}",
        escape_path(module),
        space_key(space),
        joined.join(".")
    )
}

fn member_id(module: &str, space: DeclSpace, segments: &[String]) -> NodeId {
    node_id(Domain::EcmaScript, &member_fqn(module, space, segments))
}

fn module_id(module: &str) -> NodeId {
    node_id(Domain::EcmaScript, &escape_path(module))
}

/// What a module specifier resolved to, with every identity probed on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleTarget {
    /// A module in this repository.
    Internal(String),
    /// A dependency, asset, builtin or host outside it.
    External(String),
    /// Nothing this build can name.
    Unresolved(UnresolvedReason),
}

/// A resolved specifier plus the probes that resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModule {
    /// Where the specifier landed.
    pub target: ModuleTarget,
    /// Every module identity probed, in probe order, hits and misses alike.
    pub candidates: Vec<NodeId>,
}

/// The bookkeeping one alias walk carries.
///
/// Two sets, because a module reached twice means two different things. On
/// the *current path* it is a cycle: the walk must stop and say so, or it
/// never ends. On a *sibling* path it is a diamond — two barrels
/// re-exporting one module, which is ordinary — and it already contributed
/// whatever it had, so reporting it as a cycle would put a false reason on a
/// perfectly normal module graph.
///
/// Both are membership questions and nothing else — no order is read off
/// either — so both are hashed. A barrel with `N` star targets asked one
/// name is `N` membership tests against a set that grows to `N`, and a linear
/// scan made that quadratic *per reference* on a corpus where one barrel
/// serves thousands of them. The candidate list stays a vector because its
/// order is the record of what this reference read, which the invalidation
/// index is built from.
struct Walk<'a> {
    /// Which declaration table every probe reads. Invariant: the reference's
    /// space does not change because the walk crossed a module.
    space: DeclSpace,
    /// The member path being looked up, likewise invariant — each module the
    /// walk enters is asked for the same name.
    segments: &'a [String],
    /// Every identity probed, in probe order, hits and misses alike.
    candidates: Vec<NodeId>,
    /// Modules on the current path, inserted on entry and removed on exit.
    path: HashSet<NodeId>,
    /// Modules some earlier branch already walked.
    explored: HashSet<NodeId>,
}

/// One local name an import introduced, with the module it reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    imported: ImportedName,
    module: ResolvedModule,
    /// Which declaration table the binding landed in. C17: `import type { T }`
    /// binds in the Type space only and is fully elided at emit, so a *value*
    /// use of that name names nothing that exists at runtime.
    space: DeclSpace,
}

/// The EcmaScript per-file scope: the binding table plus the facts about this
/// file that resolution consults.
///
/// A binding table and not a flat import map, because F2 is unavoidable:
/// `import parse from './p'` binds an **arbitrary** local name to `./p`'s
/// `default` export, so the call site `parse()` carries no information about
/// the definition's name. Go can build a candidate FQN from the reference text
/// alone; EcmaScript cannot, ever, for an imported binding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EcmaScope {
    /// This file's module path — the root of every FQN it contributes.
    module: String,
    /// The file's module semantics, already decided against the nearest
    /// `package.json`.
    ///
    /// Carried here because the scope is *all* the resolver's second phase
    /// receives: rebuilding a header from the path alone would lose what the
    /// file itself said, and A5/A6 make the kind decide candidate generation
    /// — an ESM specifier resolves exactly or fails, a CommonJS one probes
    /// extensions and index files. Losing it makes `a.mjs` probe like
    /// CommonJS and invent edges Node would not create.
    kind: ModuleKind,
    /// Local name → what it binds.
    bindings: HashMap<String, Binding>,
    /// Class name declared in this file → the names of its declared
    /// supertypes, in source order.
    supers: HashMap<String, Vec<Vec<String>>>,
}

/// The EcmaScript resolver. One value per dialect; the dialect changes only
/// the probe list, never the linking rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EcmaResolver {
    dialect: Dialect,
}

/// The resolver JavaScript files are linked with.
pub const JS_RESOLVER: EcmaResolver = EcmaResolver {
    dialect: Dialect::JavaScript,
};

/// The resolver TypeScript files are linked with.
pub const TS_RESOLVER: EcmaResolver = EcmaResolver {
    dialect: Dialect::TypeScript,
};

impl EcmaResolver {
    /// The module kind that governs a file: what the file itself said, or —
    /// when it said nothing — the nearest `package.json` `"type"`.
    fn module_kind(&self, cfg: &EcmaConfig, header: &EcmaHeader) -> ModuleKind {
        match header.module_kind {
            ModuleKind::Undecided => cfg.module_kind_for(&header.rel_path),
            decided => decided,
        }
    }

    /// Resolve one module specifier and record every identity probed.
    pub fn resolve_module(
        &self,
        cfg: &EcmaConfig,
        header: &EcmaHeader,
        specifier: Option<&str>,
        probe: &dyn SymbolProbe,
    ) -> ResolvedModule {
        let Some(spec) = specifier else {
            // C8/F14: the argument is an arbitrary expression. Nothing was
            // probed, because there is nothing to probe.
            return ResolvedModule {
                target: ModuleTarget::Unresolved(UnresolvedReason::DynamicModuleSpecifier),
                candidates: Vec::new(),
            };
        };
        let kind = self.module_kind(cfg, header);
        match specifier::resolve(cfg, &header.rel_path, spec, kind, self.dialect) {
            Spec::External(package) => ResolvedModule {
                target: ModuleTarget::External(package),
                candidates: Vec::new(),
            },
            Spec::Unresolved(reason) => ResolvedModule {
                target: ModuleTarget::Unresolved(reason),
                candidates: Vec::new(),
            },
            Spec::Candidates { paths, fallback } => {
                // Probed one at a time, stopping at the first hit: the
                // candidate list must be exactly what was read, or the
                // invalidation index it feeds wakes this reference for edits
                // that could not have changed its answer.
                let mut candidates = Vec::with_capacity(paths.len());
                for path in &paths {
                    let id = module_id(path);
                    candidates.push(id);
                    if probe.probe(&id).is_some() {
                        return ResolvedModule {
                            target: ModuleTarget::Internal(path.clone()),
                            candidates,
                        };
                    }
                }
                // Every probe missed. A configured alias or a `baseUrl` is an
                // overlay on the `node_modules` walk, so the declared
                // dependency underneath it is still the answer.
                let target = match fallback {
                    Some(package) => ModuleTarget::External(package),
                    None => ModuleTarget::Unresolved(self.classify_miss(cfg, &paths)),
                };
                ResolvedModule { target, candidates }
            }
        }
    }

    /// Why every candidate missed.
    ///
    /// A file that exists on disk but carries an extension no
    /// [`crate::model::Lang`] owns is real code this build never indexed —
    /// `.tsx`, `.jsx`, `.mts`, `.cts`. Reporting `ModuleNotFound` for it would
    /// blame the repository for a gap in the tool, so it is
    /// [`UnresolvedReason::TierTwoLanguage`], which is what that reason means.
    fn classify_miss(&self, cfg: &EcmaConfig, paths: &[String]) -> UnresolvedReason {
        for path in paths {
            let unowned = path
                .rsplit('/')
                .next()
                .and_then(|f| f.rsplit_once('.'))
                .is_some_and(|(_, ext)| UNOWNED_CODE_EXTENSIONS.contains(&ext));
            if unowned && cfg.root.join(path).is_file() {
                return UnresolvedReason::TierTwoLanguage;
            }
        }
        UnresolvedReason::ModuleNotFound
    }

    /// Build the per-file binding table.
    fn build_scope(
        &self,
        cfg: &EcmaConfig,
        header: &EcmaHeader,
        refs: &[Reference],
        probe: &dyn SymbolProbe,
    ) -> EcmaScope {
        let mut bindings: HashMap<String, Binding> = HashMap::new();
        for import in &header.imports {
            if import.bindings.is_empty() {
                continue; // a side-effect import binds nothing
            }
            let module = self.resolve_module(cfg, header, import.specifier.as_deref(), probe);
            for binding in &import.bindings {
                bindings.insert(
                    binding.local.clone(),
                    Binding {
                        imported: binding.imported.clone(),
                        module: module.clone(),
                        space: binding.space,
                    },
                );
            }
        }

        // F9/C29: a written `extends` clause is a resolvable reference and a
        // prerequisite for `this.m()` and `super.m()`. Collected from the
        // file's own `Inherit` references, so no second traversal is needed
        // and the extractor still does no linking.
        let mut supers: HashMap<String, Vec<Vec<String>>> = HashMap::new();
        for r in refs {
            if r.kind != RefKind::Inherit || r.target.root != TargetRoot::Name {
                continue;
            }
            let Some(encloser) = &r.enclosing else {
                continue;
            };
            let (_, path) = split_space_tag(&encloser.path, DeclSpace::Value);
            let Some(class) = path.last() else {
                continue;
            };
            supers
                .entry(class.clone())
                .or_default()
                .push(r.target.segments.clone());
        }

        EcmaScope {
            module: header.rel_path.clone(),
            kind: self.module_kind(cfg, header),
            bindings,
            supers,
        }
    }

    /// The outcome and probes for a reference that names a whole module.
    fn module_outcome(&self, module: &ResolvedModule) -> Resolution {
        let outcome = match &module.target {
            ModuleTarget::Internal(path) => Outcome::Resolved(module_id(path)),
            ModuleTarget::External(package) => Outcome::External(package.clone()),
            ModuleTarget::Unresolved(reason) => Outcome::Unresolved(reason.clone()),
        };
        Resolution {
            outcome,
            candidates: module.candidates.clone(),
        }
    }

    /// Look one exported name up in a resolved module.
    ///
    /// `segments` is the whole member path taken from that module: the first
    /// element is the exported name, the rest are members of whatever it
    /// names — `ns.sub.parse()` is depth three and stays distinguishable from
    /// a qualified name.
    fn lookup_in_module(
        &self,
        cfg: &EcmaConfig,
        module: &ResolvedModule,
        space: DeclSpace,
        segments: &[String],
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let path = match &module.target {
            ModuleTarget::Internal(path) => path,
            _ => return self.module_outcome(module),
        };
        let mut walk = Walk {
            space,
            segments,
            candidates: module.candidates.clone(),
            // The module the walk starts in is already on the path: `a`
            // starring `b` starring `a` has to end, and it ends here.
            path: HashSet::from([module_id(path)]),
            explored: HashSet::new(),
        };
        let outcome = self.lookup_exported(cfg, path, probe, &mut walk, 0);
        Resolution {
            outcome,
            candidates: walk.candidates,
        }
    }

    /// One module's export table, followed through whatever aliases it holds.
    ///
    /// The three probes are ordered as they always were — whole path, head,
    /// star — and each may now land on an alias, which is followed instead of
    /// being reported as the answer. Every probe is still recorded, hit or
    /// miss, including the ones taken inside a followed module.
    fn lookup_exported(
        &self,
        cfg: &EcmaConfig,
        path: &str,
        probe: &dyn SymbolProbe,
        walk: &mut Walk,
        depth: usize,
    ) -> Outcome<NodeId, String> {
        let (space, segments) = (walk.space, walk.segments);
        if segments.is_empty() {
            return Outcome::Resolved(module_id(path));
        }
        if depth >= MAX_ALIAS_HOPS {
            return Outcome::Unresolved(UnresolvedReason::AliasCycle);
        }
        // The whole path first: a namespace member (`A.B.C.f`) and a static
        // member (`C.m`) are both one identity, not a lookup plus a walk.
        let full = member_id(path, space, segments);
        walk.candidates.push(full);
        match probe.probe(&full) {
            Some(Entry::Alias { target }) => {
                return self.follow_alias(full, target, probe, walk);
            }
            Some(Entry::Set(_)) => return Outcome::Unresolved(UnresolvedReason::AmbiguousExport),
            Some(_) => return Outcome::Resolved(full),
            None => {}
        }
        // The head alone: whether the *exported name* exists decides whether
        // this is "the module has no such export" or "the export exists and
        // its member needs a type".
        let head = member_id(path, space, &segments[..1]);
        let head_entry = if segments.len() > 1 {
            walk.candidates.push(head);
            probe.probe(&head)
        } else {
            None
        };
        match head_entry {
            Some(Entry::Definition {
                kind: DefKind::Module | DefKind::Type,
                ..
            }) => return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
            // An alias head with members left to walk needs the target's
            // *type*, exactly as any other head does: the member path
            // continues past an identity this walk can name but not open.
            Some(_) => return Outcome::Unresolved(UnresolvedReason::NeedsTypeInference),
            None => {}
        }
        // B5/B11: the module carries a star re-export. Its targets are known,
        // so the name set can be enumerated by re-entering them.
        let star = member_id(path, space, &[STAR_EXPORT.to_string()]);
        walk.candidates.push(star);
        match probe.probe(&star) {
            Some(Entry::Alias { target }) => self.walk_stars(cfg, &[target], probe, walk, depth),
            Some(Entry::Set(targets)) => self.walk_stars(cfg, &targets, probe, walk, depth),
            // A star marker forwarding nothing this build can name: a
            // CommonJS spread (C9), or a star from outside the repository.
            // The set is still un-enumerable, which is what the reason says.
            Some(_) => Outcome::Unresolved(UnresolvedReason::WildcardImport),
            None => Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
        }
    }

    /// Walk an alias chain to the definition at its end.
    ///
    /// `from` is the alias node itself. If the chain dead-ends — a re-export
    /// of a name the target module does not declare — that alias is the
    /// answer: it is a real node, the reference really does name it, and
    /// reporting the site unresolved would lose an edge the graph has.
    /// A *cycle* is different: nothing at the end exists to fall back to, and
    /// the walk says so.
    ///
    /// The chain is linear, so its own `seen` list is enough and the walk's
    /// shared path is left alone — an alias followed here must not make a
    /// module look visited to a sibling star branch.
    fn follow_alias(
        &self,
        from: NodeId,
        target: NodeId,
        probe: &dyn SymbolProbe,
        walk: &mut Walk,
    ) -> Outcome<NodeId, String> {
        let mut anchor = from;
        let mut current = target;
        let mut seen: Vec<NodeId> = vec![from];
        for _ in 0..MAX_ALIAS_HOPS {
            if seen.contains(&current) {
                return Outcome::Unresolved(UnresolvedReason::AliasCycle);
            }
            seen.push(current);
            walk.candidates.push(current);
            match probe.probe(&current) {
                Some(Entry::Alias { target }) => {
                    anchor = current;
                    current = target;
                }
                // One name forwarded to several identities is the ambiguity
                // the reason is for.
                Some(Entry::Set(_)) => {
                    return Outcome::Unresolved(UnresolvedReason::AmbiguousExport);
                }
                Some(_) => return Outcome::Resolved(current),
                None => return Outcome::Resolved(anchor),
            }
        }
        Outcome::Unresolved(UnresolvedReason::AliasCycle)
    }

    /// Look one name up in every module a star export forwards.
    ///
    /// This is ES `ResolveExport`'s star case, bounded: one answer resolves,
    /// two different answers are the ambiguity `AmbiguousExport` names, and
    /// none — with every target enumerated — is an honest
    /// `NoMatchingDefinition`. `WildcardImport` survives for the case that
    /// still deserves it: a target this build could not enumerate.
    fn walk_stars(
        &self,
        cfg: &EcmaConfig,
        targets: &[NodeId],
        probe: &dyn SymbolProbe,
        walk: &mut Walk,
        depth: usize,
    ) -> Outcome<NodeId, String> {
        let mut found: Option<NodeId> = None;
        let mut unenumerable = false;
        let mut cycle = false;
        for target in targets {
            if walk.path.contains(target) {
                cycle = true; // on the current path: a genuine loop
                continue;
            }
            if walk.explored.contains(target) {
                // A diamond, not a loop. It contributed what it had the first
                // time it was walked, and `found` already holds it.
                continue;
            }
            let Some(path) = cfg.module_paths.get(target).cloned() else {
                // A module the store holds no path for — not scanned, or
                // scanned by another track. Its names cannot be listed.
                unenumerable = true;
                continue;
            };
            walk.path.insert(*target);
            let outcome = self.lookup_exported(cfg, &path, probe, walk, depth + 1);
            walk.path.remove(target);
            walk.explored.insert(*target);
            match outcome {
                Outcome::Resolved(id) => match found {
                    // Two stars reaching one definition is one answer, not a
                    // disagreement.
                    Some(prev) if prev != id => {
                        return Outcome::Unresolved(UnresolvedReason::AmbiguousExport);
                    }
                    _ => found = Some(id),
                },
                Outcome::Unresolved(UnresolvedReason::AliasCycle) => cycle = true,
                Outcome::Unresolved(UnresolvedReason::WildcardImport) => unenumerable = true,
                _ => {}
            }
        }
        match found {
            Some(id) => Outcome::Resolved(id),
            None if cycle => Outcome::Unresolved(UnresolvedReason::AliasCycle),
            None if unenumerable => Outcome::Unresolved(UnresolvedReason::WildcardImport),
            None => Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
        }
    }

    /// A reference whose root is a plain name.
    fn resolve_named(
        &self,
        cfg: &EcmaConfig,
        scope: &EcmaScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let segments = &r.target.segments;
        let Some(head) = segments.first() else {
            return unresolved(UnresolvedReason::NeedsExpressionType);
        };
        let space = r.space;

        // 1. An imported binding. This is the mechanism, not an optimisation:
        //    `raw_target` does not contain the target's name (F2).
        if let Some(binding) = scope.bindings.get(head) {
            // C17: an `import type` binding is elided from the emitted
            // JavaScript, so a site that *survives* erasure — a call, a
            // construction, a `class D extends B` — cannot name it, and
            // following it into the exporting module would record a runtime
            // edge for code TypeScript rejects. A site that is erased too may:
            // C20's `typeof X` is a type query that reads the **Value** space
            // from a type position, and it is exactly what a type-only import
            // is for. So the binding's space gates the *runtime* references
            // and only those; the reference's own space still says which table
            // to read.
            if binding.space == DeclSpace::Type
                && space == DeclSpace::Value
                && r.kind != RefKind::TypeUse
            {
                return unresolved(UnresolvedReason::NoMatchingDefinition);
            }
            let rest = &segments[1..];
            let path: Vec<String> = match &binding.imported {
                ImportedName::Named(exported) => {
                    let mut p = vec![exported.clone()];
                    p.extend_from_slice(rest);
                    p
                }
                // B2: the candidate is the module's `default` export, never
                // the arbitrary local name the import chose.
                ImportedName::Default => {
                    let mut p = vec!["default".to_string()];
                    p.extend_from_slice(rest);
                    p
                }
                // F4/C2: a namespace or whole-module binding puts the module's
                // own export names one segment further out.
                ImportedName::Namespace | ImportedName::Whole | ImportedName::All => rest.to_vec(),
            };
            // C3: `const Parser = require('./m')` names the *value*
            // `module.exports` holds, which is that module's `default` export
            // under ESM interop — not the module object. Only when nothing is
            // exported under `default` is the module itself the answer.
            if path.is_empty() && binding.imported == ImportedName::Whole {
                let whole = self.lookup_in_module(
                    cfg,
                    &binding.module,
                    space,
                    &["default".to_string()],
                    probe,
                );
                if matches!(whole.outcome, Outcome::Resolved(_) | Outcome::External(_)) {
                    return whole;
                }
            }
            return self.lookup_in_module(cfg, &binding.module, space, &path, probe);
        }

        // 2. This module's own declarations. E2: a non-exported module-level
        //    binding is still a node, because references *inside* the file can
        //    name it and those are the edges `impact` needs.
        let mut candidates = Vec::new();
        let full = member_id(&scope.module, space, segments);
        candidates.push(full);
        if probe.probe(&full).is_some() {
            return Resolution {
                outcome: Outcome::Resolved(full),
                candidates,
            };
        }
        if segments.len() > 1 {
            let head_id = member_id(&scope.module, space, &segments[..1]);
            candidates.push(head_id);
            match probe.probe(&head_id) {
                Some(Entry::Definition {
                    kind: DefKind::Module | DefKind::Type,
                    ..
                }) => {
                    return Resolution {
                        outcome: Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
                        candidates,
                    };
                }
                Some(_) => {
                    return Resolution {
                        outcome: Outcome::Unresolved(UnresolvedReason::NeedsTypeInference),
                        candidates,
                    };
                }
                None => {}
            }
        }

        // 3. The universe scope, consulted last so a declaration of the same
        //    name always wins. D11: `External`, never `Unresolved`.
        let external = if space == DeclSpace::Type {
            globals::lib_type_key(head).or_else(|| globals::external_key(head))
        } else {
            globals::external_key(head)
        };
        if let Some(key) = external {
            return Resolution {
                outcome: Outcome::External(key.to_string()),
                candidates,
            };
        }

        // 4. The other half of the universe scope: a name a *package* injects
        //    into it. Consulted after the host's half, so a host global is
        //    the host's whatever a dependency also puts in scope, and after
        //    steps 1 and 2, so any import or declaration of the name wins.
        //
        //    The head decides, exactly as it does for the host half above:
        //    `console.log` is `External` rather than a type question, and
        //    `vi.fn` is in vitest rather than a type this resolver failed to
        //    infer. What follows the head is inside the package the head
        //    names, and neither half of the universe scope has to look at it.
        //
        //    `Unresolved`, never `External`: the reference stays in both
        //    terms of the rate. See [`globals::INJECTED`] for why the reason
        //    is the one it is.
        if globals::injected_by(head, |package| cfg.declares_ambient(&scope.module, package))
            .is_some()
        {
            return Resolution {
                outcome: Outcome::Unresolved(UnresolvedReason::UnknownPackage),
                candidates,
            };
        }

        let reason = if segments.len() > 1 {
            // The head is neither imported, nor declared here, nor global:
            // it is a value whose type this resolver does not compute.
            UnresolvedReason::NeedsTypeInference
        } else {
            UnresolvedReason::NoMatchingDefinition
        };
        Resolution {
            outcome: Outcome::Unresolved(reason),
            candidates,
        }
    }

    /// The class a reference sits inside, from its encloser.
    fn enclosing_owner<'a>(&self, enclosing: &'a Option<Encloser>) -> Option<&'a [String]> {
        let path = &enclosing.as_ref()?.path;
        let (_, path) = split_space_tag(path, DeclSpace::Value);
        (path.len() >= 2).then(|| &path[..path.len() - 1])
    }

    /// Walk a class and its written supertypes, probing one member on each.
    ///
    /// `owner` is the member-owner chain the reference sits in, so its last
    /// segment is [`PROTOTYPE`] for an instance member and the class itself
    /// for a static one — which is precisely the difference between the two
    /// lookups, and the reason staticness has to be in the chain rather than
    /// in a facet the FQN cannot read.
    ///
    /// `skip_self` is `super.`: the same walk, starting one hop up.
    ///
    /// The walk follows only supertypes **this module declares**, checked by
    /// probing the base's own identity. A base reached through an import
    /// lives in a file whose members this scope cannot name, and stopping
    /// there is what makes [`UnresolvedReason::UnindexedSupertype`] true when
    /// it is reported: a supertype that *is* indexed is traversed, not blamed.
    fn walk_members(
        &self,
        scope: &EcmaScope,
        r: &Reference,
        owner: &[String],
        skip_self: bool,
        probe: &dyn SymbolProbe,
    ) -> Result<Resolution, (Vec<NodeId>, UnresolvedReason)> {
        let (class_path, on_prototype) = split_prototype(owner);
        let Some(class) = class_path.last().cloned() else {
            return Err((Vec::new(), UnresolvedReason::NeedsReceiverType));
        };
        let container = &class_path[..class_path.len() - 1];
        let mut candidates = Vec::new();
        let mut current = class;
        let mut hops = 0usize;
        loop {
            if !(skip_self && hops == 0) {
                let mut path = container.to_vec();
                path.push(current.clone());
                if on_prototype {
                    path.push(PROTOTYPE.to_string());
                }
                path.extend_from_slice(&r.target.segments);
                let id = member_id(&scope.module, r.space, &path);
                candidates.push(id);
                if probe.probe(&id).is_some() {
                    return Ok(Resolution {
                        outcome: Outcome::Resolved(id),
                        candidates,
                    });
                }
            }
            hops += 1;
            // No written heritage: the lookup is complete and the member is
            // absent, which is a different report from an unindexed base.
            let Some(base) = scope
                .supers
                .get(&current)
                .and_then(|bases| bases.first())
                .and_then(|segments| segments.last())
                .cloned()
            else {
                return Err((candidates, UnresolvedReason::NoMatchingDefinition));
            };
            if hops >= MAX_SUPER_HOPS || base == current {
                return Err((candidates, UnresolvedReason::UnindexedSupertype));
            }
            let mut base_path = container.to_vec();
            base_path.push(base.clone());
            let base_id = member_id(&scope.module, DeclSpace::Value, &base_path);
            candidates.push(base_id);
            if probe.probe(&base_id).is_none() {
                return Err((candidates, UnresolvedReason::UnindexedSupertype));
            }
            current = base;
        }
    }

    /// F6: `this.m()` resolved against the lexically enclosing class and the
    /// supertypes this module declares.
    ///
    /// **A decision, recorded rather than discovered.** `this` is dynamic — a
    /// method extracted and invoked elsewhere has a different receiver — so a
    /// hit here is the most likely target and not a proof, and
    /// `Outcome::Resolved` means "verified". It is taken anyway, and both case
    /// studies ask for it by name: the TypeScript study lists `this.m()` inside
    /// class `C` among the "two cheap wins that are *not* type inference",
    /// beside `super.m()`, and the JavaScript study asks for the choice to be
    /// made explicitly rather than discovered. This is the explicit choice.
    ///
    /// It stays a *lexical* lookup: the class the reference is written inside,
    /// then a bounded walk up heritage clauses written in this same file. A
    /// receiver whose type is inferred rather than written never reaches here
    /// — that is [`UnresolvedReason::NeedsReceiverType`], and it stays large.
    fn resolve_this(
        &self,
        scope: &EcmaScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let Some(owner) = self.enclosing_owner(&r.enclosing) else {
            return unresolved(UnresolvedReason::NeedsReceiverType);
        };
        if r.target.segments.is_empty() {
            return unresolved(UnresolvedReason::NeedsReceiverType);
        }
        match self.walk_members(scope, r, owner, false, probe) {
            Ok(res) => res,
            Err((candidates, reason)) => Resolution {
                outcome: Outcome::Unresolved(reason),
                candidates,
            },
        }
    }

    /// F7: `super.m()` resolved through the written `extends` heritage.
    ///
    /// A bounded walk up a statically-written chain, which is the other cheap
    /// win that is *not* type inference. It stops at the first supertype this
    /// build did not index, and says so.
    fn resolve_super(
        &self,
        cfg: &EcmaConfig,
        scope: &EcmaScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let Some(owner) = self.enclosing_owner(&r.enclosing) else {
            return unresolved(UnresolvedReason::NeedsReceiverType);
        };
        let (class_path, on_prototype) = split_prototype(owner);
        let Some(class) = class_path.last().cloned() else {
            return unresolved(UnresolvedReason::NeedsReceiverType);
        };
        if r.target.segments.is_empty() {
            // `super(...)`: the base constructor. Named by the heritage
            // reference itself, which is already an edge of its own.
            return unresolved(UnresolvedReason::NeedsReceiverType);
        }
        let (mut candidates, reason) = match self.walk_members(scope, r, owner, true, probe) {
            Ok(res) => return res,
            Err(parts) => parts,
        };
        // A base reached through an import: probe the member on the imported
        // definition before giving up.
        if let Some(bases) = scope.supers.get(&class)
            && let Some(segs) = bases.first()
            && let Some(head) = segs.first()
            && let Some(binding) = scope.bindings.get(head)
        {
            let exported = match &binding.imported {
                ImportedName::Named(n) => n.clone(),
                ImportedName::Default => "default".to_string(),
                _ => head.clone(),
            };
            let mut path = vec![exported];
            path.extend_from_slice(&segs[1..]);
            if on_prototype {
                path.push(PROTOTYPE.to_string());
            }
            path.extend_from_slice(&r.target.segments);
            let mut res = self.lookup_in_module(cfg, &binding.module, r.space, &path, probe);
            candidates.append(&mut res.candidates);
            return match res.outcome {
                Outcome::Resolved(id) => Resolution {
                    outcome: Outcome::Resolved(id),
                    candidates,
                },
                _ => Resolution {
                    outcome: Outcome::Unresolved(UnresolvedReason::UnindexedSupertype),
                    candidates,
                },
            };
        }
        Resolution {
            outcome: Outcome::Unresolved(reason),
            candidates,
        }
    }

    /// A re-export site: `export { a } from './m'`, `export * from './m'`.
    fn resolve_export(
        &self,
        cfg: &EcmaConfig,
        header: &EcmaHeader,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let specifier = match (&r.target.root, r.target.segments.first()) {
            (TargetRoot::Name, Some(spec)) => Some(spec.as_str()),
            _ => None,
        };
        let module = self.resolve_module(cfg, header, specifier, probe);
        match r.target.segments.get(1) {
            // B7: a named re-export names one thing in the requested module.
            Some(name) => {
                self.lookup_in_module(cfg, &module, r.space, std::slice::from_ref(name), probe)
            }
            // B5/B6: a star names the module itself.
            None => self.module_outcome(&module),
        }
    }

    /// The whole of resolution for one reference.
    fn resolve_ref(
        &self,
        cfg: &EcmaConfig,
        scope: &EcmaScope,
        header: &EcmaHeader,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        // D3/D4/D5: checked before any candidate is generated. A name some
        // enclosing block binds is not a node by design, so linking it would
        // emit a wrong edge — strictly worse than an unresolved reference,
        // because a miss is counted and a wrong edge is not. Empty candidates
        // are contract-legal here and only here: the verdict is decidable from
        // one file, so no definition edit anywhere can change it.
        if r.locally_bound {
            return unresolved(UnresolvedReason::LocalBinding);
        }
        // C26/F11: a JSX element whose name is lowercase and undotted is an
        // *intrinsic* — a host element checked against `JSX.IntrinsicElements`,
        // never a binding in this repository. The extractor states it as the
        // one thing the core `Reference` can carry that no other site
        // produces: a `Call` in the **Type** space, which is where the
        // language looks the name up. Exactly Node's builtin case one level
        // over, and `External` rather than `Unresolved` for the same reason
        // `node:fs` is: nothing here is missing.
        if r.kind == RefKind::Call && r.space == DeclSpace::Type {
            return Resolution {
                outcome: Outcome::External("jsx:intrinsic".to_string()),
                candidates: Vec::new(),
            };
        }
        match r.kind {
            RefKind::Import => {
                let specifier = match (&r.target.root, r.target.segments.first()) {
                    (TargetRoot::Name, Some(spec)) => Some(spec.as_str()),
                    _ => None,
                };
                let module = self.resolve_module(cfg, header, specifier, probe);
                self.module_outcome(&module)
            }
            RefKind::Export => self.resolve_export(cfg, header, r, probe),
            _ => match &r.target.root {
                TargetRoot::Name => self.resolve_named(cfg, scope, r, probe),
                TargetRoot::This { .. } => self.resolve_this(scope, r, probe),
                TargetRoot::Super { .. } => self.resolve_super(cfg, scope, r, probe),
                // F5: `obj[name]()`, `f().m()`. The operand is an expression,
                // and its type is what would be needed.
                TargetRoot::Expr => unresolved(UnresolvedReason::NeedsExpressionType),
            },
        }
    }

    /// The canonical FQN for one definition. See the module header for the
    /// grammar and its four invariants.
    fn fqn(&self, header: &EcmaHeader, owner: &[String], def: &Definition) -> Option<Fqn> {
        // A1/E12: the file *is* the module, and its FQN is the bare path. A
        // module FQN never carries the definition separator, so the container
        // and definition namespaces cannot collide.
        if def.kind == DefKind::Module && owner.is_empty() && def.name == header.rel_path {
            return Some(Fqn::new(escape_path(&header.rel_path)));
        }
        if def.name.is_empty() {
            return None; // E11: a computed name has no static name to give
        }
        let (space, rest) = split_space_tag(owner, def.space);
        let mut segments: Vec<String> = rest.to_vec();
        segments.push(def.name.clone());
        Some(Fqn::new(member_fqn(&header.rel_path, space, &segments)))
    }

    /// The module a specifier names, decided without the symbol table.
    ///
    /// `resolve_module` settles its candidate list by *probing*, and the
    /// definition phase runs against a probe that predates the event — on a
    /// cold scan, an empty one — so it would call every specifier a miss.
    /// The filesystem is the oracle instead, which is the same one the walk
    /// used to decide what to scan.
    ///
    /// A candidate that exists on disk but was not scanned yields a target
    /// no node carries, and the walk falls back to the alias itself, so the
    /// disagreement costs an edge's precision and never its honesty.
    fn alias_module(&self, cfg: &EcmaConfig, header: &EcmaHeader, spec: &str) -> Option<String> {
        let kind = self.module_kind(cfg, header);
        match specifier::resolve(cfg, &header.rel_path, spec, kind, self.dialect) {
            Spec::Candidates { paths, .. } => {
                paths.into_iter().find(|p| cfg.root.join(p).is_file())
            }
            Spec::External(_) | Spec::Unresolved(_) => None,
        }
    }

    /// The identity one `export *` entry forwards to — always one, even when
    /// the specifier leaves the repository.
    ///
    /// A star contributes the *module* it forwards and the walk re-enters
    /// that module to look the name up there. When the module cannot be
    /// entered, that fact is itself the answer and has to survive as a
    /// target: a star entry contributing *nothing* left the alias node
    /// indistinguishable from a fully enumerable one, so a barrel that
    /// stars both `./local` and a dependency reported a name absent from
    /// `./local` as `NoMatchingDefinition` — a search that read part of the
    /// export set claiming it had read all of it.
    ///
    /// The identity a departing star contributes is never a module path:
    /// both the external key and the un-nameable marker carry a `:`, which no
    /// FQN this resolver builds may hold (see
    /// `a_colon_in_a_file_name_cannot_forge_an_external_key`).
    /// [`EcmaConfig::module_paths`] therefore never answers for one, and
    /// [`EcmaResolver::walk_stars`] records it as the un-enumerable branch it
    /// is. A local star alongside it still speaks: a name both a local and a
    /// dependency star carried would be ambiguous and unusable under
    /// `ResolveExport`, so a corpus that compiles cannot mean two answers
    /// here.
    fn star_target(&self, cfg: &EcmaConfig, header: &EcmaHeader, spec: &str) -> Fqn {
        let unnameable = || Fqn::new(format!("{UNENUMERABLE_MODULE}{spec}"));
        let kind = self.module_kind(cfg, header);
        match specifier::resolve(cfg, &header.rel_path, spec, kind, self.dialect) {
            Spec::Candidates { paths, fallback } => {
                match paths.into_iter().find(|p| cfg.root.join(p).is_file()) {
                    Some(path) => Fqn::new(escape_path(&path)),
                    // Every candidate missed. A configured alias or a
                    // `baseUrl` is an overlay on the `node_modules` walk, so
                    // the declared dependency underneath it is still what the
                    // star reaches — the same fallback `resolve_module` takes.
                    None => match fallback {
                        Some(package) => Fqn::new(format!("external:{package}")),
                        None => unnameable(),
                    },
                }
            }
            Spec::External(package) => Fqn::new(format!("external:{package}")),
            Spec::Unresolved(_) => unnameable(),
        }
    }

    /// What an export alias forwards to.
    ///
    /// The extractor emitted the alias node from an [`ExportEntry`] and kept
    /// the entry's raw text — a specifier and an imported name — without
    /// linking either. This is where that raw text becomes an identity, in
    /// the same phase and from the same inputs as the alias's own FQN, so the
    /// symbol table carries the forward before any reference probes it.
    ///
    /// A star entry contributes the *module* it forwards, because its name
    /// set is not a fact about this file; the walk in `lookup_in_module`
    /// re-enters that module and looks the name up there. Every star entry
    /// contributes one — see [`EcmaResolver::star_target`] — including the
    /// ones that leave the repository, whose un-enumerability is a fact the
    /// merged target list has to carry. Every other entry contributes the
    /// single identity it names.
    fn alias_targets(&self, cfg: &EcmaConfig, header: &EcmaHeader, def: &Definition) -> Vec<Fqn> {
        // Only a module-level export alias forwards. A nested one is a member
        // of something, and members are not re-exported.
        if def.kind != DefKind::Alias || !def.owner.is_empty() || def.name.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<Fqn> = Vec::new();
        let mut push = |fqn: Fqn| {
            if !out.iter().any(|f| f.as_str() == fqn.as_str()) {
                out.push(fqn);
            }
        };
        let is_star = def.name == STAR_EXPORT;
        for entry in &header.exports {
            if entry.space != def.space {
                continue;
            }
            match &entry.export_name {
                // The bare star, and the CommonJS spread that stands for it.
                // A spread carries no specifier at all — not even one this
                // build could fail to name — so it contributes no target and
                // the node stays a bare marker, which keeps `WildcardImport`
                // true for exactly the case that names nothing to enter.
                None => {
                    if !is_star {
                        continue;
                    }
                    let Some(spec) = entry.module_request.as_deref() else {
                        continue;
                    };
                    push(self.star_target(cfg, header, spec));
                }
                Some(name) => {
                    if is_star || name != &def.name {
                        continue;
                    }
                    match entry.module_request.as_deref() {
                        // A re-export: the target lives in another module.
                        Some(spec) => {
                            let Some(path) = self.alias_module(cfg, header, spec) else {
                                // External or unresolvable. Storing nothing
                                // leaves the existing module-level answer to
                                // speak, which is already the honest one.
                                continue;
                            };
                            match &entry.import_name {
                                Some(ImportedName::Named(imported)) => push(Fqn::new(member_fqn(
                                    &path,
                                    entry.space,
                                    std::slice::from_ref(imported),
                                ))),
                                // B2: the module's `default`, never a local
                                // name chosen at the import site.
                                Some(ImportedName::Default) => push(Fqn::new(member_fqn(
                                    &path,
                                    entry.space,
                                    &["default".to_string()],
                                ))),
                                // `export * as ns from './m'` names the
                                // module object itself.
                                Some(
                                    ImportedName::Namespace
                                    | ImportedName::Whole
                                    | ImportedName::All,
                                ) => push(Fqn::new(escape_path(&path))),
                                None => {}
                            }
                        }
                        // A local export under another name: `export { p as
                        // parse }`, `export default foo`. The target is this
                        // module's own declaration.
                        None => {
                            if let Some(local) = &entry.local_name {
                                push(Fqn::new(member_fqn(
                                    &header.rel_path,
                                    entry.space,
                                    std::slice::from_ref(local),
                                )));
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// A15: a minified bundle is not source.
///
/// One 3 MB single-line file contributes thousands of junk definitions and
/// destroys the resolution rate's meaning. The directories that usually hold
/// them are skipped by the walk; this catches the ones committed beside real
/// source.
fn is_minified(rel_path: &str) -> bool {
    let file = rel_path.rsplit('/').next().unwrap_or(rel_path);
    file.contains(".min.")
}

/// The shared body of both [`Resolver`] impls.
///
/// A macro rather than a blanket impl because `Resolver<L>` is parameterised
/// by the language and the two languages differ in exactly one thing the
/// resolver reads — the dialect, which is carried on the resolver value, not
/// on `L`. Every method below is identical for both.
macro_rules! ecma_resolver_impl {
    ($lang:ty) => {
        impl Resolver<$lang> for EcmaResolver {
            fn config(&self, root: &Path, _files: &FileIndex) -> Result<EcmaConfig, LayoutError> {
                // Never an error. A repository with no `package.json` is a
                // legitimate pile of scripts, and the honest answer for it —
                // relative specifiers resolve, bare ones are unknown packages
                // — is a measurement rather than an abort.
                Ok(project::build(root))
            }

            fn config_digest(&self, cfg: &EcmaConfig) -> Vec<u8> {
                // Exactly the manifest facts, and nothing the scan learns as
                // it runs. A project with no manifests at all returns an empty
                // digest: it states no opinion, so it can never invalidate a
                // store.
                if cfg.scopes.is_empty() && cfg.ts_projects.is_empty() {
                    return Vec::new();
                }
                cfg.digest.clone()
            }

            fn declared_container(
                &self,
                _cfg: &EcmaConfig,
                _header: &EcmaHeader,
            ) -> Option<(String, String)> {
                // A1: a module is a file, not a directory, and its name is its
                // path. No file decides a name any *other* file needs, so
                // there is nothing for the driver to carry between them.
                None
            }

            fn learn_containers(
                &self,
                cfg: &mut EcmaConfig,
                names: &std::collections::HashMap<String, String>,
            ) {
                // No container *name* is learned here — see
                // `declared_container`, a module's name is its path and no
                // file decides another's. What is learned is the inverse of
                // the module keyspace: identity back to path.
                //
                // Following `export *` needs it. The star's targets are
                // stored as module identities, and re-entering one to look a
                // name up in it needs that module's path, which no hash can
                // give back. The driver refreshes this between the phases, so
                // a cold scan sees every module phase 1 has just written.
                cfg.module_paths = names
                    .iter()
                    .map(|(import_path, name)| {
                        (node_id(Domain::EcmaScript, import_path), name.clone())
                    })
                    .collect();
            }

            fn owns_file(&self, _cfg: &EcmaConfig, rel_path: &str) -> bool {
                !is_minified(rel_path)
            }

            fn def_fqn(
                &self,
                _cfg: &EcmaConfig,
                header: &EcmaHeader,
                owner: &[String],
                def: &Definition,
                _probe: &dyn SymbolProbe,
            ) -> Option<Fqn> {
                self.fqn(header, owner, def)
            }

            fn def_alias_targets(
                &self,
                cfg: &EcmaConfig,
                header: &EcmaHeader,
                def: &Definition,
                _probe: &dyn SymbolProbe,
            ) -> Vec<Fqn> {
                self.alias_targets(cfg, header, def)
            }

            fn index_keys(&self, _cfg: &EcmaConfig, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
                // Empty, and not because EcmaScript needs no extra keys: E13
                // wants export aliases reachable in the node keyspace. The
                // driver never calls this method, so an alias is emitted as a
                // `DefKind::Alias` *definition* by the extractor instead and
                // reaches the keyspace through phase 1. Returning keys here as
                // well would create the same identity twice.
                Vec::new()
            }

            fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
                // An EcmaScript FQN starts with the module path, so a
                // collision is always inside one file — where a repeated name
                // is declaration merging (C2, C7, C8), a TypeScript overload
                // set (F1), or a redeclaration whose last writer wins (D7).
                // All of those are one entity.
                //
                // The exception is real: `class C { m(){} static m(){} }` is
                // two distinct members, and the FQN cannot separate them
                // because `static` is a facet and `Encloser::as_definition`
                // zeroes facets (invariant 4). Reporting that pair as merged
                // would hide a genuine collision, so `static` is the one thing
                // checked.
                a.facets.contains(DefFacets::STATIC) == b.facets.contains(DefFacets::STATIC)
            }

            fn scope(
                &self,
                cfg: &EcmaConfig,
                file: &FileFacts<$lang>,
                probe: &dyn SymbolProbe,
            ) -> EcmaScope {
                self.build_scope(cfg, &file.header, &file.refs, probe)
            }

            fn link_kinds(&self) -> &'static [RefKind] {
                // Still empty, and the driver now does read it: an empty list
                // means it runs no supertype phase over this track, which is
                // the right answer twice over. The star chain does not need
                // one — `GetExportedNames` is a
                // fixed point over the module graph, and rather than a global
                // pass per hop, each lookup walks the chain it actually needs
                // (`lookup_exported`) from targets the definition phase
                // already resolved. The walk is bounded and cycle-guarded, so
                // it terminates where a fixed point would have converged.
                //
                // What remains genuinely un-enumerable — a CommonJS spread, a
                // star from outside the repository — keeps `WildcardImport`,
                // which is the reason's own definition.
                //
                // `extends` is the other candidate and is deliberately absent:
                // `walk_members` already reads a class's heritage clause out
                // of the file being resolved, and vue-core measures five
                // `UnindexedSupertype` occurrences in total. Declaring the kind
                // would buy a phase for five references and a rewrite of that
                // walk; it is a migration to make when the number says so.
                &[]
            }

            fn resolve(
                &self,
                cfg: &EcmaConfig,
                scope: &EcmaScope,
                r: &Reference,
                probe: &dyn SymbolProbe,
            ) -> Resolution {
                // The header is rebuilt from the scope: `resolve` does not
                // receive one, and the two facts a specifier needs — the
                // module path and its kind — are both in the scope already.
                // The kind is carried rather than re-derived: `EcmaHeader`'s
                // default is `Undecided`, which would send every `.mjs` file
                // back to its `package.json` and, absent a `"type"` field,
                // resolve its specifiers as CommonJS.
                let header = EcmaHeader {
                    rel_path: scope.module.clone(),
                    module_kind: scope.kind,
                    ..EcmaHeader::default()
                };
                self.resolve_ref(cfg, scope, &header, r, probe)
            }
        }
    };
}

ecma_resolver_impl!(JsLang);
ecma_resolver_impl!(TsLang);

/// Every identity a file's definitions would be filed under, for tests and
/// for the integration suite. Not used by the driver.
pub fn definition_fqn(header: &EcmaHeader, def: &Definition) -> Option<Fqn> {
    JS_RESOLVER.fqn(header, &def.owner, def)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RefTarget, Span};
    use std::collections::HashSet;

    fn header(rel: &str) -> EcmaHeader {
        EcmaHeader {
            rel_path: rel.to_string(),
            ..EcmaHeader::default()
        }
    }

    fn def(kind: DefKind, name: &str, owner: &[&str], space: DeclSpace) -> Definition {
        Definition {
            kind,
            name: name.to_string(),
            owner: owner.iter().map(|s| s.to_string()).collect(),
            space,
            facets: DefFacets::default(),
            params: None,
            span: Span {
                byte_start: 0,
                byte_end: 0,
                line: 1,
            },
        }
    }

    fn fqn_of(rel: &str, d: &Definition) -> String {
        JS_RESOLVER
            .fqn(&header(rel), &d.owner, d)
            .expect("nameable")
            .into_string()
    }

    #[test]
    fn a_module_fqn_is_the_bare_path_and_a_member_fqn_carries_one_hash() {
        let module = def(DefKind::Module, "src/a.ts", &[], DeclSpace::Namespace);
        assert_eq!(fqn_of("src/a.ts", &module), "src/a.ts");
        let parse = def(DefKind::Function, "parse", &[], DeclSpace::Value);
        assert_eq!(fqn_of("src/a.ts", &parse), "src/a.ts#value:parse");
        // A1's invariant, stated as a test: the two namespaces cannot collide
        // because one contains `#` and the other never does.
        assert!(!fqn_of("src/a.ts", &module).contains('#'));
        assert_eq!(fqn_of("src/a.ts", &parse).matches('#').count(), 1);
    }

    #[test]
    fn two_files_in_one_directory_are_two_identities() {
        // A1: module is a file, not a directory. Go's `pkgpath.Name` collides
        // here, which is the whole reason for the path-rooted grammar.
        let parse = def(DefKind::Function, "parse", &[], DeclSpace::Value);
        assert_ne!(fqn_of("src/a.js", &parse), fqn_of("src/b.js", &parse));
    }

    #[test]
    fn the_declaration_space_separates_what_typescript_permits() {
        // C1: `interface Foo {}` beside `const Foo = 1` is legal and is two
        // symbols. Without the space they are one node and one silently wins.
        let value = def(DefKind::Const, "Foo", &[], DeclSpace::Value);
        let ty = def(DefKind::Type, "Foo", &[], DeclSpace::Type);
        let ns = def(DefKind::Module, "Foo", &[], DeclSpace::Namespace);
        assert_eq!(fqn_of("m.ts", &value), "m.ts#value:Foo");
        assert_eq!(fqn_of("m.ts", &ty), "m.ts#type:Foo");
        assert_eq!(fqn_of("m.ts", &ns), "m.ts#ns:Foo");
    }

    #[test]
    fn a_space_tag_in_the_owner_chain_survives_an_encloser() {
        // Invariant 4. `Encloser::as_definition` keeps `owner` and `name` and
        // throws the space away, so an interface member's edge source would
        // land in the Value space and dangle. The reserved owner segment is
        // how the space rides along.
        let member = def(DefKind::Method, "m", &["I"], DeclSpace::Type);
        assert_eq!(fqn_of("m.ts", &member), "m.ts#type:I.m");

        let encloser = Encloser {
            path: vec![SPACE_TAG_TYPE.into(), "I".into(), "m".into()],
            kind: DefKind::Method,
        };
        let as_def = encloser.as_definition().expect("nameable");
        assert_eq!(as_def.space, DeclSpace::Value, "the core zeroes it");
        assert_eq!(fqn_of("m.ts", &as_def), "m.ts#type:I.m");
    }

    #[test]
    fn an_encloser_and_its_definition_build_the_same_fqn() {
        // The invariant the whole grammar is shaped around: an edge and the
        // node it starts at cannot disagree.
        for (owner, name, kind, space) in [
            (vec![], "parse", DefKind::Function, DeclSpace::Value),
            (vec!["C"], "m", DefKind::Method, DeclSpace::Value),
            (vec!["N", "Inner"], "f", DefKind::Function, DeclSpace::Value),
        ] {
            let d = def(kind, name, &owner, space);
            let mut path: Vec<String> = owner.iter().map(|s| s.to_string()).collect();
            path.push(name.to_string());
            let encloser = Encloser { path, kind };
            let as_def = encloser.as_definition().expect("nameable");
            assert_eq!(fqn_of("m.ts", &d), fqn_of("m.ts", &as_def), "{name}");
        }
    }

    #[test]
    fn static_is_not_in_the_fqn_and_mergeable_says_so() {
        // Invariant 4 says the FQN may not read a facet. That is why the
        // static/instance split rides in the *owner chain* instead — but two
        // definitions that reach this function with the same owner and
        // differing only in `STATIC` are still not one entity, and saying
        // they are would hide a genuine collision.
        let instance = def(DefKind::Method, "m", &["C"], DeclSpace::Value);
        let mut stat = instance.clone();
        stat.facets = DefFacets::STATIC;
        assert_eq!(fqn_of("m.ts", &instance), fqn_of("m.ts", &stat));
        assert!(!Resolver::<JsLang>::mergeable(
            &JS_RESOLVER,
            &instance,
            &stat
        ));
        // As the extractor writes them, they are two identities.
        let written = def(DefKind::Method, "m", &["C", PROTOTYPE], DeclSpace::Value);
        assert_ne!(fqn_of("m.ts", &written), fqn_of("m.ts", &stat));

        // Two declarations of one entity — an overload set, declaration
        // merging, a CommonJS redeclaration — are merged, not counted as a
        // collision.
        let again = instance.clone();
        assert!(Resolver::<JsLang>::mergeable(
            &JS_RESOLVER,
            &instance,
            &again
        ));
    }

    #[test]
    fn the_grammar_escapes_so_arbitrary_export_names_cannot_collide() {
        // B12: ES2022 permits `export { x as "my-name" }` with any string.
        // A hash collision here would be silent.
        let odd = def(DefKind::Alias, "a#b.c", &[], DeclSpace::Value);
        let fqn = fqn_of("m.ts", &odd);
        assert_eq!(fqn, "m.ts#value:a%23b%2Ec");
        assert_eq!(fqn.matches('#').count(), 1);

        let nested = def(DefKind::Method, "c", &["a#b"], DeclSpace::Value);
        assert_ne!(fqn_of("m.ts", &nested), fqn);
    }

    #[test]
    fn a_colon_in_a_file_name_cannot_forge_an_external_key() {
        // `crate::pipeline` keys a dependency under `external:<pkg>` and
        // rests that on no FQN containing a colon. A POSIX file name may hold
        // one, so a repository with a file named `external:npm:foo.js` would
        // otherwise hash its module node and the external package
        // `npm:foo.js` to one identity — silently, as every hash collision is.
        let module = def(
            DefKind::Module,
            "external:npm:foo.js",
            &[],
            DeclSpace::Namespace,
        );
        let fqn = fqn_of("external:npm:foo.js", &module);
        assert_eq!(fqn, "external%3Anpm%3Afoo.js");
        assert!(!fqn.contains(':'));
        assert_ne!(fqn, "external:npm:foo.js");
    }

    #[test]
    fn the_prototype_segment_separates_an_instance_member_from_a_static_one() {
        let instance = def(DefKind::Method, "m", &["C", PROTOTYPE], DeclSpace::Value);
        let stat = def(DefKind::Method, "m", &["C"], DeclSpace::Value);
        assert_eq!(fqn_of("m.ts", &instance), "m.ts#value:C.prototype.m");
        assert_eq!(fqn_of("m.ts", &stat), "m.ts#value:C.m");
        assert_ne!(fqn_of("m.ts", &instance), fqn_of("m.ts", &stat));

        // And it survives an encloser, so an edge out of an instance method
        // starts at the node that method is.
        let encloser = Encloser {
            path: vec!["C".into(), PROTOTYPE.into(), "m".into()],
            kind: DefKind::Method,
        };
        let as_def = encloser.as_definition().expect("nameable");
        assert_eq!(fqn_of("m.ts", &as_def), fqn_of("m.ts", &instance));

        let owner = vec!["C".to_string(), PROTOTYPE.to_string()];
        assert_eq!(split_prototype(&owner), (&owner[..1], true));
        assert_eq!(split_prototype(&owner[..1]), (&owner[..1], false));
    }

    #[test]
    fn a_hash_in_a_file_name_cannot_forge_a_member_fqn() {
        let module = def(DefKind::Module, "src/a#b.js", &[], DeclSpace::Namespace);
        assert_eq!(fqn_of("src/a#b.js", &module), "src/a%23b.js");
        assert!(!fqn_of("src/a#b.js", &module).contains('#'));
    }

    #[test]
    fn splitting_a_space_tag_falls_back_to_the_definitions_own_space() {
        let owner = vec!["C".to_string()];
        assert_eq!(
            split_space_tag(&owner, DeclSpace::Value),
            (DeclSpace::Value, &owner[..])
        );
        let tagged = vec![SPACE_TAG_NS.to_string(), "N".to_string()];
        let (space, rest) = split_space_tag(&tagged, DeclSpace::Value);
        assert_eq!(space, DeclSpace::Namespace);
        assert_eq!(rest, ["N"]);
    }

    #[test]
    fn a_minified_bundle_is_not_this_scans_file() {
        assert!(!Resolver::<JsLang>::owns_file(
            &JS_RESOLVER,
            &EcmaConfig::default(),
            "vendor/jquery.min.js"
        ));
        assert!(Resolver::<JsLang>::owns_file(
            &JS_RESOLVER,
            &EcmaConfig::default(),
            "src/index.js"
        ));
    }

    #[test]
    fn a_manifest_less_project_states_no_opinion_in_its_digest() {
        // An empty digest never invalidates a store. A project with no
        // manifests decides nothing, so it must not be able to wipe anything.
        let empty = EcmaConfig::default();
        assert!(Resolver::<JsLang>::config_digest(&JS_RESOLVER, &empty).is_empty());
    }

    #[test]
    fn a_locally_bound_reference_is_local_binding_with_no_candidates() {
        let scope = EcmaScope::default();
        let r = Reference {
            kind: RefKind::Call,
            space: DeclSpace::Value,
            raw_target: "save".into(),
            target: RefTarget {
                root: TargetRoot::Name,
                segments: vec!["save".into()],
            },
            locally_bound: true,
            argc: Some(0),
            arg_types: None,
            enclosing: None,
            span: Span {
                byte_start: 0,
                byte_end: 0,
                line: 1,
            },
        };
        let table: HashSet<NodeId> = HashSet::new();
        let res =
            Resolver::<JsLang>::resolve(&JS_RESOLVER, &EcmaConfig::default(), &scope, &r, &table);
        assert_eq!(
            res.outcome,
            Outcome::Unresolved(UnresolvedReason::LocalBinding)
        );
        assert!(res.candidates.is_empty());
    }
}
