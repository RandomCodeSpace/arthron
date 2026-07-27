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
//! identifier. Both are escaped (`%`→`%25`, `#`→`%23`; a `seg` additionally
//! escapes `.`→`%2E` and `:`→`%3A`), so the single unescaped `#` is
//! positional and a module FQN and a definition FQN can never collide in
//! `hash(domain, fqn)`. The escape matters even though it is rare: ES2022
//! permits `export { x as "my-name" }` with any string, including one holding
//! a `#`, and a hash collision is silent.
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
//! # What is *not* implemented, and why the reasons stay honest
//!
//! A bare `export * from './x'` cannot be followed: its export set is ES
//! `GetExportedNames`, a fixed point over the module graph, and the core
//! declares [`Resolver::link_kinds`] for exactly that phase but never calls
//! it. So a star re-export contributes one marker node ([`STAR_EXPORT`]) and a
//! name that misses in a module carrying one is
//! [`UnresolvedReason::WildcardImport`] — the reason's own definition. It is
//! *not* reported as [`UnresolvedReason::AmbiguousExport`], which would claim
//! we enumerated two sources and found them to disagree; we enumerated none.

use std::collections::HashMap;
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

/// Escape a module path for the FQN grammar.
fn escape_path(path: &str) -> String {
    path.replace('%', "%25").replace('#', "%23")
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

/// One local name an import introduced, with the module it reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    imported: ImportedName,
    module: ResolvedModule,
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
        module: &ResolvedModule,
        space: DeclSpace,
        segments: &[String],
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let path = match &module.target {
            ModuleTarget::Internal(path) => path,
            _ => return self.module_outcome(module),
        };
        let mut candidates = module.candidates.clone();
        if segments.is_empty() {
            return Resolution {
                outcome: Outcome::Resolved(module_id(path)),
                candidates,
            };
        }
        // The whole path first: a namespace member (`A.B.C.f`) and a static
        // member (`C.m`) are both one identity, not a lookup plus a walk.
        let full = member_id(path, space, segments);
        candidates.push(full);
        if probe.probe(&full).is_some() {
            return Resolution {
                outcome: Outcome::Resolved(full),
                candidates,
            };
        }
        // The head alone: whether the *exported name* exists decides whether
        // this is "the module has no such export" or "the export exists and
        // its member needs a type".
        let head = member_id(path, space, &segments[..1]);
        let head_entry = if segments.len() > 1 {
            candidates.push(head);
            probe.probe(&head)
        } else {
            None
        };
        let outcome = match head_entry {
            Some(Entry::Definition {
                kind: DefKind::Module | DefKind::Type,
                ..
            }) => Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
            Some(_) => Outcome::Unresolved(UnresolvedReason::NeedsTypeInference),
            None => {
                // B5/B11: the module carries a star re-export whose name set
                // is a fixed point over the module graph. Saying
                // `NoMatchingDefinition` would claim the lookup table was
                // complete, and it was not.
                let star = member_id(path, space, &[STAR_EXPORT.to_string()]);
                candidates.push(star);
                if probe.probe(&star).is_some() {
                    Outcome::Unresolved(UnresolvedReason::WildcardImport)
                } else {
                    Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
                }
            }
        };
        Resolution {
            outcome,
            candidates,
        }
    }

    /// A reference whose root is a plain name.
    fn resolve_named(
        &self,
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
                let whole =
                    self.lookup_in_module(&binding.module, space, &["default".to_string()], probe);
                if matches!(whole.outcome, Outcome::Resolved(_) | Outcome::External(_)) {
                    return whole;
                }
            }
            return self.lookup_in_module(&binding.module, space, &path, probe);
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

    /// F6: `this.m()` resolved against the lexically enclosing class.
    ///
    /// **A decision, recorded rather than discovered.** `this` is dynamic — a
    /// method extracted and invoked elsewhere has a different receiver — so a
    /// hit here is the most likely target and not a proof, and
    /// `Outcome::Resolved` means "verified". It is taken anyway, and narrowly:
    /// only when the member is declared in the class the reference is
    /// lexically inside, which is a lexical lookup and not type inference.
    /// Widening it to the prototype chain of an unindexed supertype would be
    /// the point at which the contract's meaning softened.
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
        let mut path = owner.to_vec();
        path.extend_from_slice(&r.target.segments);
        let id = member_id(&scope.module, r.space, &path);
        if probe.probe(&id).is_some() {
            return Resolution {
                outcome: Outcome::Resolved(id),
                candidates: vec![id],
            };
        }
        // The receiver type is known and in-repository and the member is not
        // in it. If the class declares a supertype this build did not index,
        // that is where the member is, and saying so is the honest report.
        let class = owner.last().cloned().unwrap_or_default();
        let reason = if scope.supers.contains_key(&class) {
            UnresolvedReason::UnindexedSupertype
        } else {
            UnresolvedReason::NoMatchingDefinition
        };
        Resolution {
            outcome: Outcome::Unresolved(reason),
            candidates: vec![id],
        }
    }

    /// F7: `super.m()` resolved through the written `extends` heritage.
    ///
    /// A bounded walk up a statically-written chain, which is the other cheap
    /// win that is *not* type inference. It stops at the first supertype this
    /// build did not index, and says so.
    fn resolve_super(
        &self,
        scope: &EcmaScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let Some(owner) = self.enclosing_owner(&r.enclosing) else {
            return unresolved(UnresolvedReason::NeedsReceiverType);
        };
        let Some(class) = owner.last() else {
            return unresolved(UnresolvedReason::NeedsReceiverType);
        };
        if r.target.segments.is_empty() {
            // `super(...)`: the base constructor. Named by the heritage
            // reference itself, which is already an edge of its own.
            return unresolved(UnresolvedReason::NeedsReceiverType);
        }
        let mut candidates = Vec::new();
        let mut current = class.clone();
        for _ in 0..MAX_SUPER_HOPS {
            let Some(bases) = scope.supers.get(&current) else {
                break;
            };
            let Some(base) = bases.first().and_then(|segs| segs.last()) else {
                break;
            };
            let mut path = vec![base.clone()];
            path.extend_from_slice(&r.target.segments);
            let id = member_id(&scope.module, r.space, &path);
            candidates.push(id);
            if probe.probe(&id).is_some() {
                return Resolution {
                    outcome: Outcome::Resolved(id),
                    candidates,
                };
            }
            // Only a base declared in this same file can be walked further;
            // one that came through an import lives in another file whose
            // heritage this scope does not hold.
            if current == *base {
                break;
            }
            current = base.clone();
        }
        // A base reached through an import: probe the member on the imported
        // definition before giving up.
        if let Some(bases) = scope.supers.get(class)
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
            path.extend_from_slice(&r.target.segments);
            let mut res = self.lookup_in_module(&binding.module, r.space, &path, probe);
            let mut all = candidates;
            all.append(&mut res.candidates);
            return match res.outcome {
                Outcome::Resolved(id) => Resolution {
                    outcome: Outcome::Resolved(id),
                    candidates: all,
                },
                _ => Resolution {
                    outcome: Outcome::Unresolved(UnresolvedReason::UnindexedSupertype),
                    candidates: all,
                },
            };
        }
        Resolution {
            outcome: Outcome::Unresolved(UnresolvedReason::UnindexedSupertype),
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
                self.lookup_in_module(&module, r.space, std::slice::from_ref(name), probe)
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
                TargetRoot::Name => self.resolve_named(scope, r, probe),
                TargetRoot::This { .. } => self.resolve_this(scope, r, probe),
                TargetRoot::Super { .. } => self.resolve_super(scope, r, probe),
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
                _cfg: &mut EcmaConfig,
                _names: &std::collections::HashMap<String, String>,
            ) {
                // Nothing to learn: see `declared_container`.
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
                // `RefKind::Export` belongs here: ES `GetExportedNames` is a
                // fixed point over the module graph and a star chain needs one
                // pass per hop. The driver never reads this list, so the fixed
                // point does not run and a name that only a star supplies is
                // reported `WildcardImport` — the reason's own definition —
                // rather than guessed at. Declaring the kinds anyway would
                // claim a phase that does not happen.
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
                let header = EcmaHeader {
                    rel_path: scope.module.clone(),
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
        // Invariant 4 has a cost, and it is paid openly rather than hidden:
        // `class C { m(){} static m(){} }` is two members with one identity.
        let instance = def(DefKind::Method, "m", &["C"], DeclSpace::Value);
        let mut stat = instance.clone();
        stat.facets = DefFacets::STATIC;
        assert_eq!(fqn_of("m.ts", &instance), fqn_of("m.ts", &stat));
        assert!(!Resolver::<JsLang>::mergeable(
            &JS_RESOLVER,
            &instance,
            &stat
        ));

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
