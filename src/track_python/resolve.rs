//! The Python resolver: all cross-file linking for Python. Never drops.
//!
//! # The FQN grammar
//!
//! One container separator and one member separator, exactly as Go uses them,
//! so the two languages' grammars read the same way even though their module
//! namespaces do not:
//!
//! ```text
//! module     pkg.sub                  (pkg/sub.py, or pkg/sub/__init__.py)
//! function   pkg.sub#f
//! class      pkg.sub#C
//! method     pkg.sub#C.m
//! nested     pkg.sub#Outer.Inner.m
//! attribute  pkg.sub#C.x
//! ```
//!
//! Four invariants, each load-bearing:
//!
//! 1. **`#` separates a container from its members; `.` only joins
//!    identifiers within one container.** This is what keeps the module
//!    `pkg.util` and the function `util` of `pkg/__init__.py` apart. Python
//!    writes both `pkg.util`, so a dots-throughout grammar gives them one
//!    [`crate::model::NodeId`] and silently merges two nodes — the single
//!    worst failure the case study found. `#` appears exactly once in a
//!    definition FQN and never in a module FQN, and a Python identifier can
//!    contain neither `#` nor `.`, so the split is unambiguous.
//! 2. **The module half is injective.** See [`crate::track_python::project`]:
//!    dotted, root-prefixed and path-shaped module names partition, so no two
//!    files can claim one module identity.
//! 3. **No arity, no signature.** Python has no compile-time overloading —
//!    defaults, `*args`, `**kwargs`, keyword-only and positional-only
//!    parameters mean arity does not discriminate a callee even in principle
//!    (G-02). Two `def f` in one module are one name, one cell, one node.
//! 4. **No occurrence-order component, and no span.** Every part of an FQN is
//!    a fact an unrelated edit cannot move: the file's path, the enclosing
//!    class chain, and the declared name. Adding a line above a definition
//!    must not renumber the graph.
//!
//! # What a resolved edge claims
//!
//! Python resolves a free variable at every execution against the module's
//! global namespace (§4.2.2). An arthron edge therefore asserts **"this site
//! names the global `pkg.mod.f`"**, not "this site invokes the function object
//! defined at `pkg/mod.py:42`". Late binding does not weaken the claim; it
//! narrows it, and it is stated here rather than buried because it is the
//! exact point where a reader's intuition and the tool's guarantee diverge.
//! A monkeypatch is recorded as its own [`RefKind::Rebind`] reference to the
//! same node rather than as a reason to downgrade every call.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::lang::{FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe};
use crate::model::{
    DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, RefTarget, Reference, TargetRoot,
    node_id,
};
use crate::track_python::extract::{ImportForm, ImportSpec, PyHeader};
use crate::track_python::lang::PyLang;
use crate::track_python::project::{
    ModPlace, PyProject, extension_module_paths, infer_roots, package_dirs, parse_pyproject,
    parse_requirements, parse_setup_py,
};
use crate::track_python::stdlib::{BUILTINS_PACKAGE, is_builtin, is_stdlib, stdlib_package};
use crate::{Outcome, UnresolvedReason};

/// How deep the base-class walk goes before giving up.
///
/// C3 linearization requires an acyclic base graph, so a correct program
/// terminates well inside this. The cap is against a *broken* one: a file that
/// declares `class A(B)` and `class B(A)` still has to produce an answer
/// rather than a stack overflow.
const MRO_DEPTH: usize = 16;

/// Python, as the shared driver's resolver.
pub struct PyResolver;

/// What one module-level name is bound to (§4.2.1).
///
/// A `Vec<Bind>` per name rather than one, because Python legitimately binds a
/// global more than once — a conditional `def`, a `try: import c_impl /
/// except ImportError: import py_impl` pair (B-16), a version-conditional
/// import (B-17) — and at runtime there is exactly one global cell. Source
/// order is candidate order, which is what the ordered-candidate mechanism
/// was built for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    /// `import a.b.c` binds `a`; `import a.b.c as x` binds the leaf (B-01/B-02).
    Module(String),
    /// `from a.b import c [as d]` binds a name *inside* a module (B-03).
    Member {
        /// The module the name is read from, absolute and dot-anchored.
        module: String,
        /// The name as written in the imported module.
        name: String,
    },
    /// Declared at module level in this file: a `def`, a `class`, an
    /// assignment, or a `global` write from inside a function (C-07).
    Own,
}

/// One file's resolution scope. The core never inspects it.
pub struct PyScope {
    /// This file's module FQN.
    module: String,
    /// The package root that owns this file, for A-06 candidate ordering.
    root: String,
    /// `__package__`: the dotted package a relative import anchors at, or
    /// `None` when the file is under no package and nothing can anchor
    /// (B-07/A-07).
    package: Option<String>,
    /// Module-level bindings, in source order per name.
    bindings: HashMap<String, Vec<Bind>>,
    /// Modules star-imported into this file, absolute and anchored (B-09/B-10).
    stars: Vec<String>,
    /// A star import that could not be anchored at all.
    star_unanchored: bool,
    /// Import clauses by `(span, raw target)`, which is what pairs a
    /// [`RefKind::Import`] reference with the clause that produced it.
    imports: HashMap<(u32, u32, String), ImportSpec>,
    /// Class owner path (`C`, `Outer.Inner`) → the bases it declares.
    bases: HashMap<String, Vec<RefTarget>>,
    /// Classes naming a metaclass, by owner path. §3.3.3 lets a metaclass
    /// `__new__` add attributes with no source site at all (F-11).
    metaclassed: HashSet<String>,
    /// Every class this file declares, by owner path. A class that is here
    /// with no `bases` entry has no bases and is therefore *fully* enumerated;
    /// a class that is not here cannot be walked at all.
    classes: HashSet<String>,
    /// `(enclosing definition path, name)` → the single type annotated on it,
    /// or `None` when two annotations disagree (E-05).
    annotations: HashMap<(String, String), Option<Vec<String>>>,
    /// `exec`, `eval` or `globals()` appears, so names may enter the module
    /// namespace with no static declaration site (C-17).
    has_dynamic_namespace: bool,
    /// `sys.path` is mutated, so absolute imports may mean something the
    /// configured roots cannot express (B-21).
    mutates_sys_path: bool,
}

/// What a candidate walk learned on the way, and could not enumerate.
///
/// The reason mapping reads this instead of guessing: each field turns one
/// otherwise-indistinguishable miss into a distinct piece of work.
#[derive(Debug, Default)]
struct Walk {
    /// The root named a definite container — a module, an imported name, or a
    /// name this file declares — so a miss is about the *member*, not about
    /// not knowing what the receiver is.
    root_typed: bool,
    /// A supertype the walk could not expand: it lives in another file, so
    /// its own bases are not readable from here.
    unindexed_supertype: bool,
    /// The root resolved to a node this file declares that is **not** a class
    /// — a module-level variable, or a function. Its members are not
    /// enumerable from any declaration, so a miss below it is the type of the
    /// root going unknown, not a definition going missing.
    opaque_root: bool,
    /// A star-imported module whose export set cannot be enumerated (B-11).
    unenumerable_star: bool,
    /// The root binds to something outside the repository.
    external: Option<String>,
    /// Modules whose namespace was searched, for the PEP 562 probe.
    searched_modules: Vec<String>,
}

impl PyResolver {
    /// The module FQN of the file a header describes.
    fn module_of(cfg: &PyProject, header: &PyHeader) -> String {
        cfg.module_fqn(&header.rel_path)
    }

    /// Anchor a possibly-relative import onto an absolute dotted module path
    /// (PEP 328, §5.4.2, B-05/B-06/B-07).
    ///
    /// `None` is B-08: the import reaches past the top-level package, which
    /// means either the source is broken or the inferred layout is wrong. The
    /// honest outcome is `ProjectLayoutUnknown` — arthron's own inference —
    /// and not `NoMatchingDefinition`, which would blame a definition.
    fn anchor(scope: &PyScope, level: u8, module: &[String]) -> Option<String> {
        let mut base: Vec<String> = if level == 0 {
            Vec::new()
        } else {
            let package = scope.package.as_ref()?;
            if package.is_empty() {
                return None; // §5.4.2: relative import beyond top-level
            }
            let mut segments: Vec<String> = package.split('.').map(str::to_string).collect();
            let up = usize::from(level) - 1;
            if up >= segments.len() {
                return None;
            }
            segments.truncate(segments.len() - up);
            segments
        };
        base.extend(module.iter().cloned());
        (!base.is_empty()).then(|| base.join("."))
    }

    /// Probe candidates in order, recording every identity read.
    ///
    /// Stops at the first hit, and `probed` holds exactly the prefix that was
    /// read — the invalidation index is built from it, so listing a candidate
    /// that was never probed would wake this reference for edits that could
    /// not have changed its outcome.
    fn probe_in_order(
        probe: &dyn SymbolProbe,
        candidates: &[String],
        probed: &mut Vec<NodeId>,
    ) -> Option<NodeId> {
        for fqn in candidates {
            let id = node_id(Domain::Python, fqn);
            if probed.contains(&id) {
                continue; // already read; reading it twice would double-count
            }
            probed.push(id);
            if probe.probe(&id).is_some() {
                return Some(id);
            }
        }
        None
    }

    /// Ordered candidates for a dotted path anchored at a module.
    ///
    /// Longest module prefix first, then shorter ones, then the whole path as
    /// a module. `import a.b.c` followed by `a.b.c.f()` therefore probes
    /// `a.b.c#f` first, which is E-07's whole point: a chain longer than two
    /// segments is resolvable whenever its prefix is a module, and collapsing
    /// it into "needs type inference" would be a lie.
    fn module_member_candidates(
        cfg: &PyProject,
        scope: &PyScope,
        module: &str,
        rest: &[String],
        out: &mut Vec<String>,
    ) {
        for split in (0..rest.len()).rev() {
            let container = join_dotted(module, &rest[..split]);
            let member = rest[split..].join(".");
            for fqn in cfg.module_fqns(&scope.root, &container) {
                out.push(format!("{fqn}#{member}"));
            }
        }
        out.extend(cfg.module_fqns(&scope.root, &join_dotted(module, rest)));
    }

    /// Ordered candidates for a member below a class, own class first and then
    /// its bases in declaration order (E-01, E-14).
    ///
    /// The MRO is walked only as far as the graph can be read: bases declared
    /// in this file are expanded transitively, and a base declared elsewhere
    /// gets exactly one probe, because a probe answers "does this identity
    /// exist" and not "what are its bases". That shortfall is recorded as
    /// [`Walk::unindexed_supertype`] rather than smoothed over — see the core
    /// gap noted on [`Resolver::link_kinds`] below.
    // Recursive: the four trailing parameters are this walk's own state, and
    // threading them reads better than a struct that would exist only here.
    #[allow(clippy::too_many_arguments)]
    fn class_member_candidates(
        cfg: &PyProject,
        scope: &PyScope,
        class_fqn: &str,
        member: &[String],
        out: &mut Vec<String>,
        walk: &mut Walk,
        seen: &mut HashSet<String>,
        depth: usize,
    ) {
        if member.is_empty() || depth > MRO_DEPTH || !seen.insert(class_fqn.to_string()) {
            return;
        }
        out.push(format!("{class_fqn}.{}", member.join(".")));
        let Some(owner) = class_fqn
            .strip_prefix(&scope.module)
            .and_then(|rest| rest.strip_prefix('#'))
        else {
            // Declared in another file: its own bases are not readable here.
            walk.unindexed_supertype = true;
            return;
        };
        if !scope.classes.contains(owner) {
            // Not a class this file declares — an attribute of one, or a name
            // that is not a class at all. Nothing to linearize, and nothing
            // that could tell us what members it has: `register = mk()` is a
            // node, but `register.tag` is a member of whatever `mk()`
            // returned.
            walk.opaque_root = true;
            return;
        }
        for base in scope.bases.get(owner).map(Vec::as_slice).unwrap_or(&[]) {
            if base.root != TargetRoot::Name {
                walk.unindexed_supertype = true;
                continue;
            }
            let mut base_fqns = Vec::new();
            let mut base_walk = Walk::default();
            Self::name_candidates(cfg, scope, &base.segments, &mut base_fqns, &mut base_walk);
            walk.unindexed_supertype |= base_walk.unindexed_supertype;
            if base_fqns.is_empty() {
                walk.unindexed_supertype = true;
            }
            for base_fqn in base_fqns.iter().filter(|f| f.contains('#')) {
                Self::class_member_candidates(
                    cfg,
                    scope,
                    base_fqn,
                    member,
                    out,
                    walk,
                    seen,
                    depth + 1,
                );
            }
        }
    }

    /// Ordered candidates for a dotted name read in this file's module block.
    ///
    /// The lookup order is §4.2.2's, minus the two steps that are provably
    /// in-file: the block check has already happened in the extractor, the
    /// enclosing-function scope is never a node, and what is left is
    /// **module bindings, then star imports, then builtins** — builtins last,
    /// which is C-02: a module-level `print = mk()` shadows the builtin, and
    /// a flat builtin list checked first would call that `External` and hide a
    /// real in-repository edge.
    fn name_candidates(
        cfg: &PyProject,
        scope: &PyScope,
        segments: &[String],
        out: &mut Vec<String>,
        walk: &mut Walk,
    ) {
        let Some(head) = segments.first() else {
            return;
        };
        let rest = &segments[1..];
        if let Some(binds) = scope.bindings.get(head) {
            walk.root_typed = true;
            for bind in binds {
                match bind {
                    Bind::Own => {
                        let base = format!("{}#{head}", scope.module);
                        if rest.is_empty() {
                            out.push(base);
                        } else {
                            let mut seen = HashSet::new();
                            Self::class_member_candidates(
                                cfg, scope, &base, rest, out, walk, &mut seen, 0,
                            );
                        }
                    }
                    Bind::Module(dotted) => {
                        Self::note_external(cfg, dotted, walk);
                        walk.searched_modules.push(dotted.clone());
                        Self::module_member_candidates(cfg, scope, dotted, rest, out);
                    }
                    Bind::Member { module, name } => {
                        Self::note_external(cfg, module, walk);
                        walk.searched_modules.push(module.clone());
                        // B-03's order, one level down: the attribute of the
                        // module first, then the submodule of the same name.
                        for module_fqn in cfg.module_fqns(&scope.root, module) {
                            let base = format!("{module_fqn}#{name}");
                            if rest.is_empty() {
                                out.push(base);
                            } else {
                                let mut seen = HashSet::new();
                                Self::class_member_candidates(
                                    cfg, scope, &base, rest, out, walk, &mut seen, 0,
                                );
                            }
                        }
                        let submodule = format!("{module}.{name}");
                        Self::module_member_candidates(cfg, scope, &submodule, rest, out);
                    }
                }
            }
            return;
        }
        // §7.11: a star import makes the source module's public names module
        // globals here. Go's `dot_imports` loop is structurally the same probe.
        walk.unenumerable_star |= scope.star_unanchored;
        for star in &scope.stars {
            if is_stdlib(top_segment(star)) || cfg.declares_dependency(top_segment(star)) {
                // Its export set is not in the graph, so a miss here proves
                // nothing about whether the name exists (B-11).
                walk.unenumerable_star = true;
            }
            for module_fqn in cfg.module_fqns(&scope.root, star) {
                let base = format!("{module_fqn}#{head}");
                out.push(if rest.is_empty() {
                    base
                } else {
                    format!("{base}.{}", rest.join("."))
                });
            }
        }
    }

    /// Note that a dotted module path names something outside the repository.
    ///
    /// Recorded rather than returned: an in-repository module of the same name
    /// still wins, because `sys.path` order can put one there, so every
    /// in-repository candidate is probed first and this only decides the
    /// outcome once they have all missed.
    fn note_external(cfg: &PyProject, dotted: &str, walk: &mut Walk) {
        if walk.external.is_some() {
            return;
        }
        let top = top_segment(dotted);
        if cfg.ext_modules.contains(dotted) {
            walk.external = Some(dotted.to_string()); // A-10
        } else if is_stdlib(top) {
            walk.external = Some(stdlib_package(top)); // B-23
        } else if cfg.declares_dependency(top) {
            walk.external = Some(top.to_string());
        }
    }

    /// Candidates read off an annotation rather than inferred (E-05).
    ///
    /// `def f(c: Client): c.send()` is resolvable with no type inference at
    /// all — read the annotation, resolve `Client` through the same binding
    /// table, walk its bases. This is the reason such a site must never be
    /// filed under `NeedsTypeInference`: the label would hide work that is
    /// already done behind one that sounds impossible.
    fn annotation_candidates(
        cfg: &PyProject,
        scope: &PyScope,
        r: &Reference,
        segments: &[String],
        out: &mut Vec<String>,
        walk: &mut Walk,
    ) {
        if segments.len() < 2 {
            return;
        }
        let block = r
            .enclosing
            .as_ref()
            .map_or_else(String::new, |e| e.path.join("."));
        let key = (block, segments[0].clone());
        let Some(Some(type_path)) = scope.annotations.get(&key) else {
            return;
        };
        let mut type_fqns = Vec::new();
        let mut type_walk = Walk::default();
        Self::name_candidates(cfg, scope, type_path, &mut type_fqns, &mut type_walk);
        if walk.external.is_none() {
            walk.external = type_walk.external;
        }
        // A module is not a type: only the definition half of the candidate
        // list can host a member.
        for type_fqn in type_fqns.iter().filter(|f| f.contains('#')) {
            walk.root_typed = true;
            let mut seen = HashSet::new();
            Self::class_member_candidates(
                cfg,
                scope,
                type_fqn,
                &segments[1..],
                out,
                walk,
                &mut seen,
                0,
            );
        }
    }

    /// Classify a reference every candidate missed.
    ///
    /// Ordered so that each reason names a *different* piece of work, which is
    /// the only thing that makes the taxonomy worth having: a bucket that
    /// collects two unrelated failures cannot be worked off.
    fn miss(
        cfg: &PyProject,
        scope: &PyScope,
        segments: &[String],
        walk: &Walk,
        probe: &dyn SymbolProbe,
        probed: &mut Vec<NodeId>,
    ) -> Outcome<NodeId, String> {
        if let Some(package) = &walk.external {
            return Outcome::External(package.clone());
        }
        let head = segments.first().map(String::as_str).unwrap_or("");
        // §4.2.2: builtins are the last scope searched, so this is reached
        // only after every binding and star candidate has missed (C-02).
        if !scope.bindings.contains_key(head) && is_builtin(head) {
            return Outcome::External(BUILTINS_PACKAGE.to_string());
        }
        // PEP 562: a module with `__getattr__` serves attributes that have no
        // declaration site. Reporting `NoMatchingDefinition` for one would be
        // false, and it would understate the corpus's real difficulty.
        for module in &walk.searched_modules {
            let lazy: Vec<String> = cfg
                .module_fqns(&scope.root, module)
                .into_iter()
                .map(|fqn| format!("{fqn}#__getattr__"))
                .collect();
            if Self::probe_in_order(probe, &lazy, probed).is_some() {
                return Outcome::Unresolved(UnresolvedReason::Generated);
            }
        }
        // A module whose top-level package is neither the standard library
        // (answered by `walk.external` above), nor a declared dependency (the
        // same), nor anything this repository declares was never indexed. The
        // member asked of it is that package's, and `NoMatchingDefinition`
        // would blame this repository for a name that was never in it. This
        // is exactly the test `missing_module` applies at an import site,
        // asked again at the use site so the two agree.
        for module in &walk.searched_modules {
            let roots = cfg.module_fqns(&scope.root, top_segment(module));
            if Self::probe_in_order(probe, &roots, probed).is_none() {
                return Outcome::Unresolved(UnresolvedReason::UnknownPackage);
            }
        }
        if walk.root_typed {
            return Outcome::Unresolved(if walk.unindexed_supertype {
                // A class chain that ran out of readable bases.
                UnresolvedReason::UnindexedSupertype
            } else if walk.opaque_root {
                // The root is a node, but not one whose members are written
                // down anywhere. That is the same missing fact as `x.m()` on
                // an unannotated local: the type of the root.
                UnresolvedReason::NeedsTypeInference
            } else {
                UnresolvedReason::NoMatchingDefinition
            });
        }
        // E-06: a dotted target whose root nothing typed is type inference and
        // nothing else — the receiver is a name with no binding and no
        // annotation. Answered before the two reasons below, because both of
        // those are about a *bare* name (could a star import have supplied it,
        // could an `exec` have created it) and neither is a claim anyone can
        // make about a member access. Letting either take a dotted target
        // would shrink the floor this track is reviewed for without linking a
        // thing.
        if segments.len() >= 2 {
            return Outcome::Unresolved(UnresolvedReason::NeedsTypeInference);
        }
        if walk.unenumerable_star {
            return Outcome::Unresolved(UnresolvedReason::WildcardImport);
        }
        if scope.has_dynamic_namespace {
            return Outcome::Unresolved(UnresolvedReason::Generated); // C-17
        }
        Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
    }

    /// Classify an import whose module could not be found.
    fn missing_module(
        cfg: &PyProject,
        scope: &PyScope,
        dotted: &str,
        level: u8,
        probe: &dyn SymbolProbe,
        probed: &mut Vec<NodeId>,
    ) -> Outcome<NodeId, String> {
        let top = top_segment(dotted);
        if cfg.ext_modules.contains(dotted) {
            return Outcome::External(dotted.to_string()); // A-10
        }
        if level > 0 {
            // A relative import is in-repository by construction, and it
            // never consults `sys.path`, so a mutation of that path says
            // nothing about why this module is missing.
            return Outcome::Unresolved(UnresolvedReason::ModuleNotFound);
        }
        if is_stdlib(top) {
            return Outcome::External(stdlib_package(top));
        }
        if cfg.declares_dependency(top) {
            return Outcome::External(top.to_string());
        }
        // B-21, and only now: resolution proceeded on roots the file itself
        // changed, so the failure is arthron's layout rather than a missing
        // module. It is asked *after* the three answers above because none of
        // them depends on the search path — `is_stdlib` reads a frozen name
        // set — and blaming the layout for `import os` would put a reference
        // arthron knows the answer to into the rate's denominator under a
        // reason naming work that does not exist.
        if scope.mutates_sys_path {
            return Outcome::Unresolved(UnresolvedReason::ProjectLayoutUnknown);
        }
        // A top-level package this repository does declare means the specifier
        // named a module inside it that does not exist; anything else is
        // outside the repository and was never indexed.
        let roots: Vec<String> = cfg.module_fqns(&scope.root, top);
        if Self::probe_in_order(probe, &roots, probed).is_some() {
            Outcome::Unresolved(UnresolvedReason::ModuleNotFound)
        } else {
            Outcome::Unresolved(UnresolvedReason::UnknownPackage)
        }
    }

    /// Classify an import reference.
    fn resolve_import(
        cfg: &PyProject,
        scope: &PyScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let key = (r.span.byte_start, r.span.byte_end, r.raw_target.clone());
        let Some(spec) = scope.imports.get(&key) else {
            // Every clause shares its span with exactly one import reference,
            // so this is unreachable; reporting it is still better than a
            // panic on a file shape nobody has seen yet.
            return unresolved(UnresolvedReason::DynamicModuleSpecifier);
        };
        let mut probed = Vec::new();
        match &spec.form {
            ImportForm::Module { path, .. } => {
                let dotted = path.join(".");
                let candidates = cfg.module_fqns(&scope.root, &dotted);
                match Self::probe_in_order(probe, &candidates, &mut probed) {
                    Some(id) => resolution(Outcome::Resolved(id), probed),
                    None => {
                        let outcome =
                            Self::missing_module(cfg, scope, &dotted, 0, probe, &mut probed);
                        resolution(outcome, probed)
                    }
                }
            }
            ImportForm::From {
                level,
                module,
                name,
                ..
            } => {
                let Some(base) = Self::anchor(scope, *level, module) else {
                    return unresolved(UnresolvedReason::ProjectLayoutUnknown); // B-08
                };
                // §7.11, verbatim: "check if the imported module has an
                // attribute by that name; if not, attempt to import a
                // submodule with that name". An ordered two-candidate probe,
                // and it is only expressible because the module and definition
                // namespaces hash apart.
                let mut candidates: Vec<String> = cfg
                    .module_fqns(&scope.root, &base)
                    .into_iter()
                    .map(|fqn| format!("{fqn}#{name}"))
                    .collect();
                candidates.extend(cfg.module_fqns(&scope.root, &format!("{base}.{name}")));
                if let Some(id) = Self::probe_in_order(probe, &candidates, &mut probed) {
                    return resolution(Outcome::Resolved(id), probed);
                }
                let module_fqns = cfg.module_fqns(&scope.root, &base);
                if Self::probe_in_order(probe, &module_fqns, &mut probed).is_none() {
                    let outcome =
                        Self::missing_module(cfg, scope, &base, *level, probe, &mut probed);
                    return resolution(outcome, probed);
                }
                // The module is here and the name is not. PEP 562 is the one
                // reading under which that is not a defect.
                let lazy: Vec<String> = module_fqns
                    .iter()
                    .map(|fqn| format!("{fqn}#__getattr__"))
                    .collect();
                let outcome = if Self::probe_in_order(probe, &lazy, &mut probed).is_some() {
                    Outcome::Unresolved(UnresolvedReason::Generated) // B-14
                } else {
                    Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
                };
                resolution(outcome, probed)
            }
            ImportForm::Star { level, module } => {
                let Some(base) = Self::anchor(scope, *level, module) else {
                    return unresolved(UnresolvedReason::ProjectLayoutUnknown);
                };
                let candidates = cfg.module_fqns(&scope.root, &base);
                match Self::probe_in_order(probe, &candidates, &mut probed) {
                    // The reference names the module, and the module is here.
                    // Whether its export set can be enumerated is a question
                    // about *later* bare names, not about this site.
                    Some(id) => resolution(Outcome::Resolved(id), probed),
                    None => {
                        let outcome =
                            Self::missing_module(cfg, scope, &base, *level, probe, &mut probed);
                        resolution(outcome, probed)
                    }
                }
            }
        }
    }

    /// The class path a `self`/`cls`/`super()` site sits inside, if any.
    fn enclosing_class(r: &Reference) -> Option<String> {
        let path = &r.enclosing.as_ref()?.path;
        (path.len() >= 2).then(|| path[..path.len() - 1].join("."))
    }

    /// `self.m()` and `cls.m()` — Python's single biggest statically
    /// resolvable call class (E-01/E-02).
    ///
    /// The lexically enclosing class is statically known, so the candidates
    /// are its own member and then each base in declaration order. `cls` may
    /// name a subclass at runtime; the static answer is an
    /// under-approximation, which is the honest one.
    fn resolve_receiver(
        cfg: &PyProject,
        scope: &PyScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let segments = &r.target.segments;
        let Some(owner) = Self::enclosing_class(r) else {
            return unresolved(UnresolvedReason::NeedsReceiverType);
        };
        if segments.len() != 1 {
            // `self.client.get()`: the receiver of `get` is the *attribute*
            // `client`, and nothing in this file states its type. §I.3 names
            // exactly this shape as belonging in the type-inference floor, and
            // `NeedsReceiverType` would claim a declared type that was never
            // written. It must equally not be flattened into `LocalBinding` —
            // `self` is a receiver whose class is known, not a local.
            return unresolved(UnresolvedReason::NeedsTypeInference);
        }
        let class_fqn = format!("{}#{owner}", scope.module);
        let mut candidates = Vec::new();
        let mut walk = Walk::default();
        let mut seen = HashSet::new();
        Self::class_member_candidates(
            cfg,
            scope,
            &class_fqn,
            segments,
            &mut candidates,
            &mut walk,
            &mut seen,
            0,
        );
        let mut probed = Vec::new();
        if let Some(id) = Self::probe_in_order(probe, &candidates, &mut probed) {
            return resolution(Outcome::Resolved(id), probed);
        }
        // E-12: a class defining `__getattr__` can serve any attribute.
        let fallback = vec![format!("{class_fqn}.__getattr__")];
        if Self::probe_in_order(probe, &fallback, &mut probed).is_some() {
            return resolution(Outcome::Unresolved(UnresolvedReason::Generated), probed);
        }
        let outcome = Outcome::Unresolved(if walk.unindexed_supertype {
            // A supertype whose own bases are unreadable from here: a fixable
            // piece of work, and named ahead of the unfixable ones.
            UnresolvedReason::UnindexedSupertype
        } else if scope.metaclassed.contains(&owner) {
            // F-11: §3.3.3 lets a metaclass `__new__` add arbitrary attributes
            // at class creation, with no source site to find. Django's
            // `ModelBase` putting `objects` and `_meta` on every model is the
            // corpus instance, and `NoMatchingDefinition` would blame the
            // repository for a name that really is there.
            UnresolvedReason::Generated
        } else {
            UnresolvedReason::NoMatchingDefinition
        });
        resolution(outcome, probed)
    }

    /// `super().m()` (E-03).
    ///
    /// The lookup starts *after* the lexically enclosing class in the MRO of
    /// `type(self)` — the runtime type, not the static one. For a
    /// single-inheritance chain the static next-in-MRO is right. Under
    /// cooperative multiple inheritance `super()` can land on a sibling branch
    /// absent from the static MRO, and `DynamicDispatch` is precisely right
    /// there: the target is chosen at runtime.
    fn resolve_super(
        cfg: &PyProject,
        scope: &PyScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let segments = &r.target.segments;
        let Some(owner) = Self::enclosing_class(r) else {
            return unresolved(UnresolvedReason::NeedsReceiverType);
        };
        if segments.is_empty() {
            return unresolved(UnresolvedReason::NeedsReceiverType);
        }
        let bases = scope.bases.get(&owner).map(Vec::as_slice).unwrap_or(&[]);
        if bases.is_empty() {
            // The implicit base is `object`, which is builtins and is real.
            return Resolution {
                outcome: Outcome::External(BUILTINS_PACKAGE.to_string()),
                candidates: Vec::new(),
            };
        }
        let mut candidates = Vec::new();
        let mut walk = Walk::default();
        for base in bases {
            if base.root != TargetRoot::Name {
                walk.unindexed_supertype = true;
                continue;
            }
            let mut base_fqns = Vec::new();
            let mut base_walk = Walk::default();
            Self::name_candidates(cfg, scope, &base.segments, &mut base_fqns, &mut base_walk);
            walk.unindexed_supertype |= base_walk.unindexed_supertype;
            for base_fqn in base_fqns.iter().filter(|f| f.contains('#')) {
                let mut seen = HashSet::new();
                Self::class_member_candidates(
                    cfg,
                    scope,
                    base_fqn,
                    segments,
                    &mut candidates,
                    &mut walk,
                    &mut seen,
                    0,
                );
            }
        }
        let mut probed = Vec::new();
        if let Some(id) = Self::probe_in_order(probe, &candidates, &mut probed) {
            return resolution(Outcome::Resolved(id), probed);
        }
        let outcome = Outcome::Unresolved(if bases.len() > 1 {
            UnresolvedReason::DynamicDispatch
        } else if walk.unindexed_supertype {
            UnresolvedReason::UnindexedSupertype
        } else {
            UnresolvedReason::NoMatchingDefinition
        });
        resolution(outcome, probed)
    }

    /// A name-rooted reference: the ordinary case, and most of the volume.
    fn resolve_name(
        cfg: &PyProject,
        scope: &PyScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let segments = &r.target.segments;
        if segments.is_empty() {
            return unresolved(UnresolvedReason::NoMatchingDefinition);
        }
        // A name some enclosing block binds is not a node by design, so the
        // *whole* target being that name is `LocalBinding` and nothing else.
        //
        // A *member of* it is not: `c.send()` names `send`, and `c` is only
        // the receiver. Filing that under `LocalBinding` would take it out of
        // both terms of the rate — which is exactly how a rate rises without
        // anything being linked — so it stays in the denominator and goes on
        // to the annotation table (E-05) and then to an honest reason.
        if r.locally_bound && segments.len() == 1 {
            return Resolution {
                outcome: Outcome::Unresolved(UnresolvedReason::LocalBinding),
                candidates: Vec::new(),
            };
        }
        let mut candidates = Vec::new();
        let mut walk = Walk::default();
        if !r.locally_bound {
            Self::name_candidates(cfg, scope, segments, &mut candidates, &mut walk);
        }
        Self::annotation_candidates(cfg, scope, r, segments, &mut candidates, &mut walk);
        let mut probed = Vec::new();
        match Self::probe_in_order(probe, &candidates, &mut probed) {
            Some(id) => resolution(Outcome::Resolved(id), probed),
            None => {
                let outcome = Self::miss(cfg, scope, segments, &walk, probe, &mut probed);
                resolution(outcome, probed)
            }
        }
    }
}

/// Join a dotted module path with further segments.
fn join_dotted(module: &str, rest: &[String]) -> String {
    if rest.is_empty() {
        return module.to_string();
    }
    format!("{module}.{}", rest.join("."))
}

/// The top-level name of a dotted module path — the distribution granularity
/// Python actually ships at.
fn top_segment(dotted: &str) -> &str {
    dotted.split('.').next().unwrap_or(dotted)
}

/// A resolution that read nothing. Legal only when the verdict is decidable
/// from one file, so no definition edit anywhere could change it.
fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

fn resolution(outcome: Outcome<NodeId, String>, candidates: Vec<NodeId>) -> Resolution {
    Resolution {
        outcome,
        candidates,
    }
}

impl Resolver<PyLang> for PyResolver {
    fn config(&self, root: &Path, files: &FileIndex) -> Result<PyProject, LayoutError> {
        let mut declared: Vec<String> = Vec::new();
        let mut dependencies: BTreeSet<String> = BTreeSet::new();
        if let Ok(src) = std::fs::read_to_string(root.join("pyproject.toml")) {
            let (roots, deps) = parse_pyproject(&src);
            declared.extend(roots);
            dependencies.extend(deps);
        }
        if let Ok(src) = std::fs::read_to_string(root.join("setup.py")) {
            declared.extend(parse_setup_py(&src));
        }
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("requirements")
                    && name.ends_with(".txt")
                    && let Ok(src) = std::fs::read_to_string(entry.path())
                {
                    dependencies.extend(parse_requirements(&src));
                }
            }
        }
        let packages = package_dirs(files.files.iter());
        let (roots, from_manifest) = if declared.is_empty() {
            (infer_roots(&packages, files.files.iter()), false)
        } else {
            declared.sort();
            declared.dedup();
            (declared, true)
        };
        let mut cfg = PyProject {
            roots,
            declared: from_manifest,
            packages,
            dependencies,
            ext_modules: BTreeSet::new(),
        };
        // A-10: a compiled module is a real module arthron will never parse,
        // so `from ._speedups import dumps` is `External` rather than a
        // missing definition. Noted from the walk because no manifest lists
        // them.
        for rel in extension_module_paths(root) {
            if let ModPlace::Rooted { dotted, .. } = cfg.place(&rel) {
                cfg.ext_modules.insert(dotted);
            }
        }
        // No `Err` arm: a Python project with no manifest at all is ordinary,
        // and an undeterminable layout is a per-reference
        // `ProjectLayoutUnknown` rather than a reason to refuse to scan.
        Ok(cfg)
    }

    fn config_digest(&self, cfg: &PyProject) -> Vec<u8> {
        cfg.digest()
    }

    fn declared_container(&self, cfg: &PyProject, header: &PyHeader) -> Option<(String, String)> {
        let module = Self::module_of(cfg, header);
        (!module.is_empty()).then(|| (module, header.module_leaf.clone()))
    }

    fn learn_containers(&self, _cfg: &mut PyProject, _names: &HashMap<String, String>) {
        // Nothing to learn. A Python module's name is decided entirely by
        // where its file sits and by the package roots, both of which every
        // file can compute for itself — unlike Go, where an unaliased import
        // binds a name written in the *imported* package's source.
    }

    fn owns_file(&self, _cfg: &PyProject, _rel_path: &str) -> bool {
        // A-06: several distributions in one tree all belong to the scan, and
        // their modules are distinguished by root rather than excluded.
        true
    }

    fn def_fqn(
        &self,
        cfg: &PyProject,
        header: &PyHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        let module = Self::module_of(cfg, header);
        if module.is_empty() {
            return None;
        }
        if def.kind == DefKind::Module {
            return Some(Fqn::new(module));
        }
        if def.name.is_empty() {
            return None; // nothing can name it
        }
        let member = if owner.is_empty() {
            def.name.clone()
        } else {
            format!("{}.{}", owner.join("."), def.name)
        };
        Some(Fqn::new(format!("{module}#{member}")))
    }

    fn index_keys(&self, _cfg: &PyProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // Python reaches every definition by its FQN alone. J9's member-name
        // index — which would let a zero-candidate `NeedsTypeInference` row
        // re-resolve once annotation-directed lookup improves — would live
        // here, but the driver never calls this method, so returning keys
        // would only look like they were being used. Recorded as a core gap
        // instead of faked.
        Vec::new()
    }

    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        // Python produces several legitimate sites for one name: a
        // conditional `def` (H-02), a `try`/`except` import pair (B-16),
        // `@property` plus `@x.setter` (F-04), a class-body attribute and the
        // `self.x = …` that also writes it (D-10), and a `global` write from
        // inside a function (C-07). At runtime there is exactly one cell, so
        // exactly one node is right and only the "one site" assumption breaks.
        //
        // Same owner and same name is the whole test. Anything else sharing an
        // FQN would mean two *modules* collided, which the grammar's
        // injectivity rules out — so answering `true` there would hide the one
        // bug this count exists to catch.
        a.name == b.name && a.owner == b.owner
    }

    fn scope(
        &self,
        cfg: &PyProject,
        file: &FileFacts<PyLang>,
        _probe: &dyn SymbolProbe,
    ) -> PyScope {
        let header = &file.header;
        let place = cfg.place(&header.rel_path);
        let module = cfg.fqn_of(&place);
        let (root, package) = match &place {
            ModPlace::Rooted { root, dotted } => {
                // B-07/PEP 366: `__package__` is `__name__` for a package and
                // `__name__.rpartition('.')[0]` for a module inside one. Both
                // give the same answer for `from . import x`; the rule that
                // produces it is different, which is why the extractor states
                // whether the file *is* a package.
                let package = if header.is_package {
                    dotted.clone()
                } else {
                    dotted
                        .rsplit_once('.')
                        .map_or_else(String::new, |(p, _)| p.to_string())
                };
                (root.clone(), Some(package))
            }
            ModPlace::Loose { .. } => (String::new(), None),
        };

        let mut scope = PyScope {
            module,
            root,
            package,
            bindings: HashMap::new(),
            stars: Vec::new(),
            star_unanchored: false,
            imports: HashMap::new(),
            bases: HashMap::new(),
            metaclassed: HashSet::new(),
            classes: HashSet::new(),
            annotations: HashMap::new(),
            has_dynamic_namespace: header.has_dynamic_namespace,
            mutates_sys_path: header.mutates_sys_path,
        };

        for def in &file.defs {
            match def.kind {
                DefKind::Module | DefKind::Alias => {}
                DefKind::Type if def.owner.is_empty() => {
                    scope.classes.insert(def.name.clone());
                    bind(&mut scope.bindings, &def.name, Bind::Own);
                }
                DefKind::Type => {
                    let mut path = def.owner.clone();
                    path.push(def.name.clone());
                    scope.classes.insert(path.join("."));
                }
                _ if def.owner.is_empty() => bind(&mut scope.bindings, &def.name, Bind::Own),
                _ => {}
            }
        }

        for spec in &header.imports {
            scope.imports.insert(
                (spec.span.byte_start, spec.span.byte_end, spec.raw_target()),
                spec.clone(),
            );
            if !spec.at_module {
                continue; // B-18: a function-local import binds a local
            }
            match &spec.form {
                ImportForm::Module { path, alias } => {
                    // §7.11, verbatim: "foo, foo.bar, and foo.bar.baz
                    // imported, foo bound locally". Without an alias the
                    // statement binds the *prefix*; with one it binds the leaf.
                    let (name, target) = match alias {
                        Some(a) => (a.clone(), path.join(".")),
                        None => match path.first() {
                            Some(first) => (first.clone(), first.clone()),
                            None => continue,
                        },
                    };
                    bind(&mut scope.bindings, &name, Bind::Module(target));
                }
                ImportForm::From {
                    level,
                    module,
                    name,
                    alias,
                } => {
                    let Some(base) = Self::anchor(&scope, *level, module) else {
                        continue;
                    };
                    let bound = alias.clone().unwrap_or_else(|| name.clone());
                    bind(
                        &mut scope.bindings,
                        &bound,
                        Bind::Member {
                            module: base,
                            name: name.clone(),
                        },
                    );
                }
                ImportForm::Star { level, module } => match Self::anchor(&scope, *level, module) {
                    Some(base) => scope.stars.push(base),
                    None => scope.star_unanchored = true,
                },
            }
        }

        // Bases are read from the header rather than paired back up with the
        // `Inherit` references beside them: a reference carries its nearest
        // nameable *encloser*, not the declaration it belongs to, so
        // reconstructing the association would be a second rule for a fact the
        // extractor already states. It also keeps a base the parse could not
        // read as a name — `class C(make_base())`, `class C(Generic[T])` — in
        // the list, where `class_member_candidates` records it as an
        // unexpanded supertype instead of silently dropping it and then
        // claiming the MRO was complete.
        for class in &header.classes {
            let owner = class.path.join(".");
            if !class.bases.is_empty() {
                scope.bases.insert(owner.clone(), class.bases.clone());
            }
            if class.has_metaclass {
                scope.metaclassed.insert(owner);
            }
        }

        for annotation in &header.annotations {
            let key = (annotation.scope.join("."), annotation.name.clone());
            scope
                .annotations
                .entry(key)
                .and_modify(|held| {
                    if held.as_ref() != Some(&annotation.type_path) {
                        // Two annotations disagree — two nested functions in
                        // one block, say. Picking one would be a guess.
                        *held = None;
                    }
                })
                .or_insert_with(|| Some(annotation.type_path.clone()));
        }

        // B-14 is deliberately absent from this struct. It is a fact about
        // the *imported* module, and the store answers only "does this
        // identity exist" — but `__getattr__` is itself a module-level `def`
        // and therefore a node, so probing for it is how the flag crosses the
        // file boundary at all. See `PyResolver::miss`.
        scope
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // F3: `self.m()` resolves against the enclosing class's MRO, which
        // needs the base-class references *resolved first*, and bases live in
        // other files. Python is therefore not stratifiable into two phases.
        //
        // The driver does not call this method today, so declaring it changes
        // nothing on its own — it states the requirement so the gap is a
        // recorded fact rather than a silent one. Until the driver drives it,
        // `class_member_candidates` expands bases declared in *this* file
        // transitively and gives a base declared elsewhere exactly one probe,
        // recording the shortfall as `UnindexedSupertype`.
        &[RefKind::Inherit]
    }

    fn resolve(
        &self,
        cfg: &PyProject,
        scope: &PyScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        // Checked before `locally_bound`, and deliberately: an `import`
        // statement *is* the binding operation, so a function-local import
        // reports `locally_bound` for the very name it introduces (B-18).
        // Routing it to `LocalBinding` would delete a real, resolvable module
        // reference from both terms of the rate.
        if r.kind == RefKind::Import {
            return Self::resolve_import(cfg, scope, r, probe);
        }
        match &r.target.root {
            TargetRoot::Name => Self::resolve_name(cfg, scope, r, probe),
            TargetRoot::This { .. } => Self::resolve_receiver(cfg, scope, r, probe),
            TargetRoot::Super { .. } => Self::resolve_super(cfg, scope, r, probe),
            // I.2: `f().m()`, `d["k"].m()`, `lst[0].m()` — a member on an
            // expression result. Genuinely needs the type of an expression.
            TargetRoot::Expr => unresolved(UnresolvedReason::NeedsExpressionType),
        }
    }
}

/// Append one binding, keeping source order and dropping exact repeats.
fn bind(bindings: &mut HashMap<String, Vec<Bind>>, name: &str, value: Bind) {
    let slot = bindings.entry(name.to_string()).or_default();
    if !slot.contains(&value) {
        slot.push(value);
    }
}

/// The Python track's scan entry point.
pub fn scan_python(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<PyLang>(
        root,
        db,
        &crate::track_python::extract::PyExtractor,
        &PyResolver,
    )
}

/// Python's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(PyLang::LANG, Lang::Python));
    assert!(matches!(PyLang::DOMAIN, Domain::Python));
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track_python::extract::extract;

    /// A project with one root at the repository root and the given packages.
    fn project(packages: &[&str]) -> PyProject {
        PyProject {
            roots: vec![String::new()],
            declared: false,
            packages: packages.iter().map(|p| (*p).to_string()).collect(),
            dependencies: BTreeSet::new(),
            ext_modules: BTreeSet::new(),
        }
    }

    /// Resolve every reference in one file against a symbol table given as
    /// FQN strings.
    fn outcomes(
        cfg: &PyProject,
        rel_path: &str,
        source: &str,
        known: &[&str],
    ) -> Vec<(String, Outcome<NodeId, String>)> {
        let table: HashSet<NodeId> = known
            .iter()
            .map(|fqn| node_id(Domain::Python, fqn))
            .collect();
        let facts = extract(rel_path, source);
        let scope = PyResolver.scope(cfg, &facts, &table);
        facts
            .refs
            .iter()
            .map(|r| {
                (
                    r.raw_target.clone(),
                    PyResolver.resolve(cfg, &scope, r, &table).outcome,
                )
            })
            .collect()
    }

    fn outcome_of(
        cfg: &PyProject,
        rel_path: &str,
        source: &str,
        known: &[&str],
        raw: &str,
    ) -> Outcome<NodeId, String> {
        outcomes(cfg, rel_path, source, known)
            .into_iter()
            .find(|(target, _)| target == raw)
            .unwrap_or_else(|| panic!("no reference `{raw}` in {rel_path}"))
            .1
    }

    fn resolved_to(fqn: &str) -> Outcome<NodeId, String> {
        Outcome::Resolved(node_id(Domain::Python, fqn))
    }

    fn reason(
        cfg: &PyProject,
        rel_path: &str,
        source: &str,
        known: &[&str],
        raw: &str,
    ) -> UnresolvedReason {
        match outcome_of(cfg, rel_path, source, known, raw) {
            Outcome::Unresolved(r) => r,
            other => panic!("`{raw}` resolved to {other:?}"),
        }
    }

    // -- the grammar ------------------------------------------------------

    #[test]
    fn a_module_and_a_same_named_function_are_different_nodes() {
        // F1, the worst finding in the case study: Python writes both
        // `pkg.util`, and a dots-throughout grammar merges them.
        let cfg = project(&["pkg"]);
        let init = extract("pkg/__init__.py", "def util():\n    pass\n");
        let module_fqn = PyResolver
            .def_fqn(&cfg, &init.header, &[], &init.defs[0], &HashSet::new())
            .unwrap();
        let function_fqn = PyResolver
            .def_fqn(&cfg, &init.header, &[], &init.defs[1], &HashSet::new())
            .unwrap();
        let submodule = extract("pkg/util.py", "");
        let submodule_fqn = PyResolver
            .def_fqn(
                &cfg,
                &submodule.header,
                &[],
                &submodule.defs[0],
                &HashSet::new(),
            )
            .unwrap();

        assert_eq!(module_fqn.as_str(), "pkg");
        assert_eq!(function_fqn.as_str(), "pkg#util");
        assert_eq!(submodule_fqn.as_str(), "pkg.util");
        assert_ne!(
            node_id(Domain::Python, function_fqn.as_str()),
            node_id(Domain::Python, submodule_fqn.as_str()),
        );
    }

    #[test]
    fn the_grammar_carries_the_class_chain_and_never_an_arity() {
        let cfg = project(&["pkg"]);
        let facts = extract(
            "pkg/mod.py",
            "class Outer:\n    class Inner:\n        def m(self, a, b):\n            pass\n",
        );
        let method = facts
            .defs
            .iter()
            .find(|d| d.name == "m")
            .expect("the method");
        let fqn = PyResolver
            .def_fqn(&cfg, &facts.header, &method.owner, method, &HashSet::new())
            .unwrap();
        assert_eq!(fqn.as_str(), "pkg.mod#Outer.Inner.m");
        // G-02: arity does not discriminate a Python callee even in principle.
        assert!(!fqn.as_str().contains('('));
        assert_eq!(method.params, None);
    }

    #[test]
    fn an_fqn_carries_exactly_one_container_separator() {
        let cfg = project(&["pkg"]);
        let facts = extract("pkg/mod.py", "class C:\n    def m(self):\n        pass\n");
        for def in &facts.defs {
            let fqn = PyResolver
                .def_fqn(&cfg, &facts.header, &def.owner, def, &HashSet::new())
                .unwrap();
            let hashes = fqn.as_str().matches('#').count();
            assert!(hashes <= 1, "{}", fqn.as_str());
            assert_eq!(hashes == 0, def.kind == DefKind::Module);
        }
    }

    // -- imports ----------------------------------------------------------

    #[test]
    fn an_absolute_import_resolves_to_the_module_it_names() {
        let cfg = project(&["pkg"]);
        assert_eq!(
            outcome_of(&cfg, "pkg/a.py", "import pkg.b\n", &["pkg.b"], "pkg.b"),
            resolved_to("pkg.b")
        );
    }

    #[test]
    fn from_import_probes_the_attribute_before_the_submodule() {
        // B-03, verbatim: "check if the imported module has an attribute by
        // that name; if not, attempt to import a submodule with that name".
        let cfg = project(&["pkg"]);
        assert_eq!(
            outcome_of(
                &cfg,
                "app.py",
                "from pkg import thing\n",
                &["pkg#thing"],
                "pkg.thing"
            ),
            resolved_to("pkg#thing")
        );
        assert_eq!(
            outcome_of(
                &cfg,
                "app.py",
                "from pkg import thing\n",
                &["pkg.thing"],
                "pkg.thing"
            ),
            resolved_to("pkg.thing")
        );
    }

    #[test]
    fn relative_imports_anchor_on_the_package_not_the_module() {
        // B-05/B-06/B-07.
        let cfg = project(&["pkg", "pkg/sub"]);
        assert_eq!(
            outcome_of(
                &cfg,
                "pkg/sub/m.py",
                "from . import x\n",
                &["pkg.sub#x"],
                ".x"
            ),
            resolved_to("pkg.sub#x")
        );
        assert_eq!(
            outcome_of(
                &cfg,
                "pkg/sub/m.py",
                "from .. import y\n",
                &["pkg#y"],
                "..y"
            ),
            resolved_to("pkg#y")
        );
        assert_eq!(
            outcome_of(
                &cfg,
                "pkg/sub/__init__.py",
                "from . import z\n",
                &["pkg.sub#z"],
                ".z"
            ),
            resolved_to("pkg.sub#z")
        );
    }

    #[test]
    fn a_relative_import_beyond_the_top_level_blames_the_layout() {
        // B-08: the source is broken or the inferred root is wrong. Either
        // way the failure is arthron's inference, not a missing definition.
        let cfg = project(&["pkg"]);
        assert_eq!(
            reason(&cfg, "pkg/m.py", "from ... import x\n", &[], "...x"),
            UnresolvedReason::ProjectLayoutUnknown
        );
    }

    #[test]
    fn the_stdlib_is_external_and_an_undeclared_dependency_is_not() {
        // B-23: Go's "no dot in the first segment" test is inverted here.
        let cfg = project(&["pkg"]);
        assert_eq!(
            outcome_of(&cfg, "pkg/m.py", "import os\n", &[], "os"),
            Outcome::External("py:std:os".to_string())
        );
        assert_eq!(
            reason(&cfg, "pkg/m.py", "import requests\n", &[], "requests"),
            UnresolvedReason::UnknownPackage
        );
        let mut declared = project(&["pkg"]);
        declared.dependencies.insert("requests".to_string());
        assert_eq!(
            outcome_of(&declared, "pkg/m.py", "import requests\n", &[], "requests"),
            Outcome::External("requests".to_string())
        );
    }

    #[test]
    fn a_lazily_exported_name_is_generated_and_not_a_missing_definition() {
        // B-14/PEP 562. `__getattr__` is a module-level `def`, so it is a
        // node, which is how the flag crosses a file boundary at all.
        let cfg = project(&["pkg"]);
        assert_eq!(
            reason(
                &cfg,
                "app.py",
                "from pkg import Lazy\n",
                &["pkg", "pkg#__getattr__"],
                "pkg.Lazy"
            ),
            UnresolvedReason::Generated
        );
        assert_eq!(
            reason(
                &cfg,
                "app.py",
                "from pkg import Gone\n",
                &["pkg"],
                "pkg.Gone"
            ),
            UnresolvedReason::NoMatchingDefinition
        );
    }

    #[test]
    fn a_function_local_import_is_never_a_local_binding() {
        // B-18: `import` *is* the binding operation, so the name it introduces
        // reads as locally bound. The module reference is still real.
        let cfg = project(&["pkg"]);
        let facts = extract("pkg/m.py", "def f():\n    import pkg.b\n");
        let import = facts
            .refs
            .iter()
            .find(|r| r.kind == RefKind::Import)
            .expect("the import reference");
        assert!(import.locally_bound, "the extractor states the fact");
        assert_eq!(
            outcome_of(
                &cfg,
                "pkg/m.py",
                "def f():\n    import pkg.b\n",
                &["pkg.b"],
                "pkg.b"
            ),
            resolved_to("pkg.b")
        );
    }

    #[test]
    fn a_function_local_import_does_not_bind_a_module_global() {
        // …and the converse: the local binding must not leak to the file.
        // The same file holds two sites spelled `json` — the import clause in
        // `f` and the call in `g` — and they are supposed to disagree, so the
        // site has to be selected by kind and not by its text.
        let cfg = project(&["pkg"]);
        let source = "def f():\n    import json\n\ndef g():\n    json()\n";
        let facts = extract("pkg/m.py", source);
        let table: HashSet<NodeId> = HashSet::new();
        let scope = PyResolver.scope(&cfg, &facts, &table);
        let outcome = |kind: RefKind| {
            let r = facts
                .refs
                .iter()
                .find(|r| r.kind == kind && r.raw_target == "json")
                .expect("a `json` site of this kind");
            PyResolver.resolve(&cfg, &scope, r, &table).outcome
        };
        // The clause itself names the module `json`, which is real.
        assert_eq!(
            outcome(RefKind::Import),
            Outcome::External("py:std:json".to_string())
        );
        // The call one function away names a module global that is not there:
        // §4.2.1 binds the import in `f`'s block and nowhere else (B-18).
        assert_eq!(
            outcome(RefKind::Call),
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
        );
    }

    #[test]
    fn a_try_except_import_pair_is_two_ordered_candidates() {
        // B-16: two bindings for one name, probed in source order.
        let cfg = project(&["pkg"]);
        let source = concat!(
            "try:\n",
            "    from pkg import c_impl as impl\n",
            "except ImportError:\n",
            "    from pkg import py_impl as impl\n",
            "\n",
            "impl()\n",
        );
        assert_eq!(
            outcome_of(&cfg, "app.py", source, &["pkg#py_impl"], "impl"),
            resolved_to("pkg#py_impl")
        );
    }

    // -- calls ------------------------------------------------------------

    #[test]
    fn a_module_prefix_longer_than_two_segments_still_resolves() {
        // E-07: `import a.b.c` binds `a`, and `a.b.c.f()` is fully resolvable.
        let cfg = project(&["a", "a/b"]);
        assert_eq!(
            outcome_of(
                &cfg,
                "app.py",
                "import a.b.c\n\na.b.c.f()\n",
                &["a.b.c", "a.b.c#f"],
                "a.b.c.f"
            ),
            resolved_to("a.b.c#f")
        );
    }

    #[test]
    fn self_dot_m_resolves_through_the_class_and_then_its_bases() {
        // E-01, the largest call-side win Python has.
        let cfg = project(&["pkg"]);
        let source = concat!(
            "class Base:\n",
            "    def render(self):\n",
            "        pass\n",
            "\n",
            "class Child(Base):\n",
            "    def go(self):\n",
            "        self.render()\n",
            "        self.own()\n",
            "    def own(self):\n",
            "        pass\n",
        );
        let known = ["pkg.mod#Base.render", "pkg.mod#Child.own"];
        assert_eq!(
            outcome_of(&cfg, "pkg/mod.py", source, &known, "self.render"),
            resolved_to("pkg.mod#Base.render")
        );
        assert_eq!(
            outcome_of(&cfg, "pkg/mod.py", source, &known, "self.own"),
            resolved_to("pkg.mod#Child.own")
        );
    }

    #[test]
    fn a_base_class_in_another_file_is_an_unindexed_supertype() {
        let cfg = project(&["pkg"]);
        let source = concat!(
            "from pkg.base import Base\n",
            "\n",
            "class Child(Base):\n",
            "    def go(self):\n",
            "        self.render()\n",
        );
        assert_eq!(
            reason(
                &cfg,
                "pkg/mod.py",
                source,
                &["pkg.base#Base"],
                "self.render"
            ),
            UnresolvedReason::UnindexedSupertype
        );
    }

    #[test]
    fn a_self_attribute_call_is_the_inference_floor_not_a_local_binding() {
        // §I.3 lists `self.callback()` — a member on an instance attribute
        // whose type nothing states — as belonging *in* `NeedsTypeInference`.
        // The honest floor this track is reviewed for: it must not become
        // `LocalBinding` (which would leave both terms of the rate) and it
        // must not become `NeedsReceiverType` (which would claim a declared
        // type nobody wrote).
        let cfg = project(&["pkg"]);
        let source = "class C:\n    def go(self):\n        self.client.get()\n";
        assert_eq!(
            reason(&cfg, "pkg/mod.py", source, &[], "self.client.get"),
            UnresolvedReason::NeedsTypeInference
        );
    }

    #[test]
    fn super_resolves_on_a_single_base_and_dispatches_dynamically_on_several() {
        // E-03.
        let cfg = project(&["pkg"]);
        let single = concat!(
            "class Base:\n",
            "    def run(self):\n",
            "        pass\n",
            "\n",
            "class Child(Base):\n",
            "    def run(self):\n",
            "        super().run()\n",
        );
        assert_eq!(
            outcome_of(
                &cfg,
                "pkg/mod.py",
                single,
                &["pkg.mod#Base.run"],
                "super().run"
            ),
            resolved_to("pkg.mod#Base.run")
        );
        let cooperative = concat!(
            "class A:\n    pass\n",
            "class B:\n    pass\n",
            "class C(A, B):\n",
            "    def run(self):\n",
            "        super().run()\n",
        );
        assert_eq!(
            reason(&cfg, "pkg/mod.py", cooperative, &[], "super().run"),
            UnresolvedReason::DynamicDispatch
        );
    }

    #[test]
    fn an_annotated_parameter_resolves_without_any_inference() {
        // E-05, and emphatically not `NeedsTypeInference`.
        let cfg = project(&["pkg"]);
        let source = concat!(
            "class Client:\n",
            "    def send(self):\n",
            "        pass\n",
            "\n",
            "def f(c: Client):\n",
            "    c.send()\n",
        );
        assert_eq!(
            outcome_of(
                &cfg,
                "pkg/mod.py",
                source,
                &["pkg.mod#Client.send"],
                "c.send"
            ),
            resolved_to("pkg.mod#Client.send")
        );
    }

    #[test]
    fn an_unannotated_receiver_is_the_only_thing_in_the_inference_bucket() {
        // E-06/I.1.
        let cfg = project(&["pkg"]);
        assert_eq!(
            reason(
                &cfg,
                "pkg/mod.py",
                "def f(c):\n    c.send()\n",
                &[],
                "c.send"
            ),
            UnresolvedReason::NeedsTypeInference
        );
    }

    #[test]
    fn a_member_of_a_local_is_never_filed_as_a_local_binding() {
        // The anti-gaming property, stated as a test: `LocalBinding` sits
        // outside *both* rate terms, so routing `c.send()` there would raise
        // the rate while linking nothing.
        let cfg = project(&["pkg"]);
        let outcomes = outcomes(
            &cfg,
            "pkg/mod.py",
            "def f(c):\n    c.send()\n    c()\n",
            &[],
        );
        let member = outcomes.iter().find(|(t, _)| t == "c.send").unwrap();
        let whole = outcomes.iter().find(|(t, _)| t == "c").unwrap();
        assert_eq!(
            member.1,
            Outcome::Unresolved(UnresolvedReason::NeedsTypeInference)
        );
        assert_eq!(whole.1, Outcome::Unresolved(UnresolvedReason::LocalBinding));
    }

    #[test]
    fn an_expression_receiver_is_an_expression_type_and_nothing_else() {
        // I.2.
        let cfg = project(&["pkg"]);
        assert_eq!(
            reason(&cfg, "pkg/mod.py", "def f():\n    g().m()\n", &[], "g().m"),
            UnresolvedReason::NeedsExpressionType
        );
    }

    #[test]
    fn a_module_level_binding_shadows_a_builtin() {
        // C-02: builtins are the last scope searched, so a flat list checked
        // first would hide a real in-repository edge.
        let cfg = project(&["pkg"]);
        let source = "def print(x):\n    pass\n\nprint(1)\n";
        assert_eq!(
            outcome_of(&cfg, "pkg/mod.py", source, &["pkg.mod#print"], "print"),
            resolved_to("pkg.mod#print")
        );
        assert_eq!(
            outcome_of(&cfg, "pkg/mod.py", "len([])\n", &[], "len"),
            Outcome::External(BUILTINS_PACKAGE.to_string())
        );
    }

    #[test]
    fn a_star_import_from_outside_the_repository_is_a_wildcard_import() {
        // B-10/B-11: the export set is not enumerable, so a miss proves
        // nothing about whether the name exists.
        let mut cfg = project(&["pkg"]);
        cfg.dependencies.insert("thirdparty".to_string());
        assert_eq!(
            reason(
                &cfg,
                "app.py",
                "from thirdparty import *\n\nthing()\n",
                &[],
                "thing"
            ),
            UnresolvedReason::WildcardImport
        );
    }

    #[test]
    fn a_star_import_from_inside_the_repository_is_enumerable() {
        let cfg = project(&["pkg"]);
        assert_eq!(
            outcome_of(
                &cfg,
                "app.py",
                "from pkg import *\n\nthing()\n",
                &["pkg", "pkg#thing"],
                "thing"
            ),
            resolved_to("pkg#thing")
        );
        assert_eq!(
            reason(
                &cfg,
                "app.py",
                "from pkg import *\n\ngone()\n",
                &["pkg"],
                "gone"
            ),
            UnresolvedReason::NoMatchingDefinition
        );
    }

    #[test]
    fn a_reexport_facade_resolves_at_the_alias_the_facade_declares() {
        // B-12: `pkg/__init__.py` doing `from .core import Foo as Foo` makes
        // `pkg.Foo` a real declaration site, and a reference to it names that
        // site. Chasing the alias one hop further needs `Entry::Alias`, which
        // the store never produces — recorded as a core gap, not faked.
        let cfg = project(&["pkg"]);
        assert_eq!(
            outcome_of(
                &cfg,
                "app.py",
                "from pkg import Foo\n",
                &["pkg#Foo"],
                "pkg.Foo"
            ),
            resolved_to("pkg#Foo")
        );
    }

    #[test]
    fn a_monkeypatch_is_recorded_as_a_rebind_of_the_same_node() {
        // H-03: a true fact about the call graph is not traded for a caveat.
        let cfg = project(&["pkg"]);
        let source = "import pkg.mod\n\npkg.mod.f = replacement\n";
        assert_eq!(
            outcome_of(
                &cfg,
                "app.py",
                source,
                &["pkg.mod", "pkg.mod#f"],
                "pkg.mod.f"
            ),
            resolved_to("pkg.mod#f")
        );
    }

    #[test]
    fn a_decorator_is_a_reference_from_the_block_around_the_definition() {
        // F-01.
        let cfg = project(&["pkg"]);
        let source = "from pkg import deco\n\n@deco\ndef f():\n    pass\n";
        assert_eq!(
            outcome_of(&cfg, "pkg/mod.py", source, &["pkg#deco"], "deco"),
            resolved_to("pkg#deco")
        );
    }

    #[test]
    fn a_base_class_reference_resolves_like_any_other_name() {
        let cfg = project(&["pkg"]);
        let source = "from pkg.base import Base\n\nclass C(Base):\n    pass\n";
        assert_eq!(
            outcome_of(&cfg, "pkg/mod.py", source, &["pkg.base#Base"], "Base"),
            resolved_to("pkg.base#Base")
        );
    }

    #[test]
    fn sys_path_mutation_blames_the_layout_rather_than_the_module() {
        // B-21.
        let cfg = project(&["pkg"]);
        let source = "import sys\nsys.path.append('vendor')\nimport vendored\n";
        assert_eq!(
            reason(&cfg, "pkg/m.py", source, &[], "vendored"),
            UnresolvedReason::ProjectLayoutUnknown
        );
    }

    #[test]
    fn a_compiled_extension_module_is_external_and_not_a_missing_module() {
        // A-10.
        let mut cfg = project(&["pkg"]);
        cfg.ext_modules.insert("pkg._speedups".to_string());
        assert_eq!(
            outcome_of(
                &cfg,
                "pkg/m.py",
                "from ._speedups import dumps\n",
                &[],
                "._speedups.dumps"
            ),
            Outcome::External("pkg._speedups".to_string())
        );
    }

    #[test]
    fn every_reference_gets_exactly_one_outcome_and_none_is_dropped() {
        let cfg = project(&["pkg"]);
        let source = concat!(
            "import os\n",
            "from pkg import thing\n",
            "from . import sibling\n",
            "from nowhere import *\n",
            "\n",
            "class C(thing.Base):\n",
            "    x: int = 0\n",
            "    def m(self, c: C):\n",
            "        self.m()\n",
            "        c.m()\n",
            "        os.path.join()\n",
            "        undefined()\n",
            "        f().g()\n",
        );
        let facts = extract("pkg/mod.py", source);
        let all = outcomes(&cfg, "pkg/mod.py", source, &["pkg", "pkg.mod#C.m"]);
        assert_eq!(all.len(), facts.refs.len());
        assert!(!all.is_empty());
    }

    // -- resolver plumbing ------------------------------------------------

    #[test]
    fn two_sites_for_one_name_are_one_node_and_a_collision_is_not() {
        let property_getter = Definition {
            kind: DefKind::Property,
            name: "x".into(),
            owner: vec!["C".into()],
            space: crate::model::DeclSpace::Value,
            facets: crate::model::DefFacets::default(),
            params: None,
            span: crate::model::Span {
                byte_start: 0,
                byte_end: 0,
                line: 1,
            },
        };
        let mut setter = property_getter.clone();
        setter.span.line = 5;
        assert!(PyResolver.mergeable(&property_getter, &setter));

        let mut elsewhere = property_getter.clone();
        elsewhere.owner = vec!["D".into()];
        assert!(!PyResolver.mergeable(&property_getter, &elsewhere));
    }

    #[test]
    fn the_config_digest_is_a_manifest_fingerprint() {
        let a = project(&["pkg"]);
        let mut b = a.clone();
        b.packages.insert("pkg/new".to_string());
        assert_eq!(PyResolver.config_digest(&a), PyResolver.config_digest(&b));
        let mut c = a.clone();
        c.roots = vec!["src".to_string()];
        assert_ne!(PyResolver.config_digest(&a), PyResolver.config_digest(&c));
    }

    #[test]
    fn python_declares_that_base_class_edges_need_a_fixed_point() {
        // F3, stated rather than assumed. The driver does not drive it yet.
        assert_eq!(PyResolver.link_kinds(), [RefKind::Inherit]);
    }

    #[test]
    fn every_probe_is_recorded_and_nothing_beyond_the_hit() {
        let cfg = project(&["pkg"]);
        let table: HashSet<NodeId> = ["pkg.b"]
            .iter()
            .map(|f| node_id(Domain::Python, f))
            .collect();
        let facts = extract("pkg/a.py", "import pkg.b\nimport pkg.missing\n");
        let scope = PyResolver.scope(&cfg, &facts, &table);
        for r in facts.refs.iter().filter(|r| r.kind == RefKind::Import) {
            let res = PyResolver.resolve(&cfg, &scope, r, &table);
            assert!(!res.candidates.is_empty(), "{}", r.raw_target);
            let hit = res
                .candidates
                .iter()
                .filter(|id| table.contains(*id))
                .count();
            match res.outcome {
                Outcome::Resolved(id) => {
                    assert_eq!(res.candidates.last(), Some(&id), "the hit ends the probe");
                    assert_eq!(hit, 1);
                }
                _ => assert_eq!(hit, 0, "a miss must have read no hit"),
            }
        }
    }

    // -- reasons that must name the right piece of work -------------------

    #[test]
    fn a_standard_library_import_is_external_even_where_the_file_edits_sys_path() {
        // The companion to B-21 above, and the half that was wrong: a file
        // that mutates `sys.path` cannot thereby stop `os` being the standard
        // library. `is_stdlib` reads a frozen name set, not the filesystem, so
        // the answer does not depend on the search path at all. Blaming the
        // layout here put a reference arthron knows the answer to into the
        // rate's denominator and named a piece of work that does not exist.
        let mut cfg = project(&["pkg"]);
        cfg.dependencies.insert("requests".to_string());
        let source = concat!(
            "import sys\n",
            "import os\n",
            "import requests\n",
            "sys.path.append('vendor')\n",
        );
        assert_eq!(
            outcome_of(&cfg, "pkg/m.py", source, &[], "os"),
            Outcome::External("py:std:os".to_string()),
        );
        assert_eq!(
            outcome_of(&cfg, "pkg/m.py", source, &[], "requests"),
            Outcome::External("requests".to_string()),
        );
    }

    #[test]
    fn a_relative_import_is_not_blamed_on_sys_path() {
        // A relative import never consults `sys.path`, so a mutation of it
        // says nothing about why the module is missing.
        let cfg = project(&["pkg"]);
        let source = "import sys\nsys.path.append('vendor')\nfrom .gone import thing\n";
        assert_eq!(
            reason(&cfg, "pkg/m.py", source, &[], ".gone.thing"),
            UnresolvedReason::ModuleNotFound,
        );
    }

    #[test]
    fn a_member_of_an_untyped_module_level_name_needs_type_inference() {
        // `register = template.Library()` then `register.tag()`. The root is
        // bound and is a node, so the old answer was `NoMatchingDefinition` —
        // which asserts that `tag` is defined nowhere. It is defined on
        // `Library`; what is missing is the *type* of `register`, and that is
        // the same work `x.m()` on an unannotated local names.
        let cfg = project(&["pkg"]);
        let source = "register = make()\n\n\ndef f():\n    register.tag()\n";
        assert_eq!(
            reason(
                &cfg,
                "pkg/m.py",
                source,
                &["pkg.m#register"],
                "register.tag"
            ),
            UnresolvedReason::NeedsTypeInference,
        );
    }

    #[test]
    fn a_member_a_known_class_does_not_declare_is_still_a_missing_definition() {
        // The contrast that keeps the fix above from swallowing the bucket:
        // when the root *is* a class this file declares, its members are
        // enumerable, and a miss really is a missing definition.
        let cfg = project(&["pkg"]);
        let source = "class C:\n    def a(self):\n        pass\n\n\ndef f():\n    C.b()\n";
        assert_eq!(
            reason(&cfg, "pkg/m.py", source, &["pkg.m#C", "pkg.m#C.a"], "C.b"),
            UnresolvedReason::NoMatchingDefinition,
        );
    }

    #[test]
    fn a_member_of_an_unindexed_third_party_module_is_an_unknown_package() {
        // `import docutils.core` is `UnknownPackage` at the import site
        // because the package is neither standard library, nor declared, nor
        // in this repository. `docutils.core.publish_parts()` is the same
        // fact, and `NoMatchingDefinition` would blame this repository for a
        // name that was never in it.
        let cfg = project(&["pkg"]);
        let source = "import docutils.core\n\n\ndef f():\n    docutils.core.publish_parts()\n";
        assert_eq!(
            reason(&cfg, "pkg/m.py", source, &[], "docutils.core.publish_parts"),
            UnresolvedReason::UnknownPackage,
        );
        assert_eq!(
            reason(&cfg, "pkg/m.py", source, &[], "docutils.core"),
            UnresolvedReason::UnknownPackage,
        );
    }

    #[test]
    fn a_member_of_an_in_repository_module_is_still_a_missing_definition() {
        // The contrast for the fix above: the module is in the repository, so
        // its namespace really was searched and a miss really is missing.
        let cfg = project(&["pkg"]);
        let source = "import pkg.util\n\n\ndef f():\n    pkg.util.gone()\n";
        assert_eq!(
            reason(
                &cfg,
                "pkg/m.py",
                source,
                &["pkg", "pkg.util"],
                "pkg.util.gone"
            ),
            UnresolvedReason::NoMatchingDefinition,
        );
    }
}
