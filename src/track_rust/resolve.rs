//! The one place a Rust [`Outcome`] is produced. Never drops.
//!
//! Rust is a **tier-2** language here: the extractor emits definitions,
//! structure and import references, and nothing else. So this resolver
//! answers exactly one question, 1073 times over the measured corpus — *what
//! does this path name?* — and its rate is an **import-resolution rate**, not
//! a call-graph one. There is no call site to dispatch and no receiver whose
//! type would have to be inferred, which is why the two reasons that dominate
//! every tier-1 track, [`UnresolvedReason::NeedsReceiverType`] and
//! [`UnresolvedReason::NeedsTypeInference`], are unreachable here rather than
//! small.
//!
//! [`UnresolvedReason::LocalBinding`] is unreachable for the same reason: it
//! is the reason a reference to a *local* carries, and tier 2 emits no
//! expression-level reference for a block to bind.
//!
//! # The path model
//!
//! One walk, from a base module through the segments:
//!
//! | Root | Base |
//! |---|---|
//! | `crate::…` | the file's crate root — the target's root file, which the manifest names |
//! | `self::…`, `mod x;` | the module the site sits in, inline `mod` blocks included |
//! | `super::…` | one module up per `super`, and nothing above a crate root |
//! | anything else | a dependency: a sibling crate's library, an outside package, or nothing |
//!
//! Then each segment is probed as a child module first and as an item of the
//! current module second, because Rust puts modules and items in namespaces
//! that a `use` path reads in that order.
//!
//! # What the rate cannot reach, recorded rather than left to be found
//!
//! - **A name re-exported through a glob.** `pub use x::*` forwards a set
//!   this scan never enumerates, so a later `use` of one of those names
//!   misses — and it misses as [`UnresolvedReason::NoMatchingDefinition`],
//!   which is the stronger claim than the truth.
//!   [`UnresolvedReason::WildcardImport`] is the weaker and truer one, and
//!   nothing here can reach it: the extractor writes no node for a glob (see
//!   [`crate::track_rust::extract`], which says why), so the resolver has no
//!   fact to probe and cannot tell a name a glob forwards from a name that is
//!   simply absent. Recorded as a shortfall rather than papered over — the
//!   measured corpus contains no `pub use …::*` at all, so a corpus that has
//!   one is what would earn the distinction.
//! - **An alias chain past its first hop.** `pub use crate::a::B;` makes `B`
//!   a real declaration site in that module, and a `use` naming it resolves
//!   *to the alias*, one hop short of the definition — a truthful answer, and
//!   the last one this track can give. Walking *through* one —
//!   `grep::searcher::Searcher`, where `searcher` is `pub extern crate
//!   grep_searcher as searcher` — would need the alias's FQN, and
//!   [`crate::lang::Entry::Alias`] carries a [`NodeId`] instead: a hash of
//!   that FQN, with no name left to compose `Searcher` onto. So this track's
//!   alias-hop ceiling is **zero**, where `track_ecma` and `track_python` set
//!   sixteen and walk, and a path that continues past an alias runs past the
//!   ceiling on its first hop — which is the clause of
//!   [`UnresolvedReason::AliasCycle`] this track fires, never the loop one.
//!   11 of the corpus's 13 unresolved references are exactly this. Following
//!   them needs the store to carry an alias's *name* beside its identity,
//!   which is a design change and not a resolver one.
//! - **`#[cfg]`.** 40 of the corpus's `mod` declarations are conditional, and
//!   every one of them is read. The union over configurations is the honest
//!   superset: a module declared under one platform and not another exists in
//!   the graph either way, and only a declaration whose file is absent under
//!   *every* configuration misses.
//! - **Macro-expanded items.** A `macro_rules!` body that declares items
//!   declares nothing this track sees, so a path into one misses — as
//!   [`UnresolvedReason::NoMatchingDefinition`], not
//!   [`UnresolvedReason::Generated`]. Separating the two needs evidence that
//!   a generator produced the name, and the only evidence this scan holds is
//!   that the name is absent, which is what a genuine miss looks like too.
//!   Claiming `Generated` on the strength of a nearby `macro_rules!` would
//!   put a guess in a column that carries facts.
//! - **A non-`pub` module-scope `use`.** It binds a name in its module all
//!   the same, and a `super::` path from a child module may name it; the
//!   extractor binds an alias for `pub` re-exports only (again, see
//!   [`crate::track_rust::extract`]), so the binding is not in the graph.
//!   Both of the corpus's [`UnresolvedReason::NoMatchingDefinition`] rows are
//!   this and nothing else — `crates/printer/src/standard.rs:1751` and
//!   `crates/regex/src/strip.rs:125`, each a `use super::{…}` naming a
//!   private import binding in the parent module — so that reason's count is
//!   two, and not the zero its own definition ("in a corpus that compiles
//!   this should mean *our* bug") would want.

use std::collections::HashMap;
use std::path::Path;

use crate::lang::{
    Entry, FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe,
};
use crate::model::{
    DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, RefTarget, Reference, TargetRoot,
    node_id,
};
use crate::track_rust::extract::{MODULE_MARK, RsHeader};
use crate::track_rust::lang::RsLang;
use crate::track_rust::project::{
    Dep, RsWorkspace, SYSROOT_CRATES, TargetKind, join_module, parent_module,
};
use crate::{Outcome, UnresolvedReason};

/// What one file's references are resolved against.
///
/// Every field is a manifest fact plus the file's path: Rust states where a
/// module sits nowhere else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RsScope {
    /// The crate root this file belongs to, as a module FQN.
    pub crate_root: String,
    /// The file's own module FQN.
    pub module: String,
    /// The package that owns the file, indexing [`RsWorkspace::packages`].
    pub package: Option<usize>,
    /// Whether the file's target is the package's library.
    ///
    /// A test, example, bench or binary links its own package's library as a
    /// dependency and names it by the package's name — which is how
    /// `crates/ignore/tests/*.rs` reaches `ignore::WalkBuilder`. A library
    /// cannot name itself that way, so the rule is off for one.
    pub in_lib: bool,
}

impl RsScope {
    /// Place one file: which crate root it sits under, which module it is,
    /// and which package's dependencies its paths may name.
    pub fn of(cfg: &RsWorkspace, rel_path: &str) -> RsScope {
        let place = cfg.place(rel_path);
        let package = cfg.package_of(rel_path);
        let in_lib = cfg
            .targets
            .iter()
            .any(|t| t.root == place.crate_root && t.kind == TargetKind::Lib);
        RsScope {
            module: join_module(&place.crate_root, &place.segments),
            crate_root: place.crate_root,
            package,
            in_lib,
        }
    }
}

/// Where a path landed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placed {
    /// It named this FQN inside the repository.
    Node(String),
    /// It left the repository at this crate.
    External(String),
    /// It could not be placed, for this reason.
    Missing(UnresolvedReason),
}

/// The Rust resolver. Stateless.
pub struct RsResolver;

/// What a site is allowed to name, read off the shape the extractor wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A `mod` declaration: relative, and carrying no `self` keyword, because
    /// nothing else the extractor writes takes that shape. It can name a
    /// module and nothing else — probing the item table for it would let
    /// `mod x;` resolve to a *function* called `x`, which is a wrong edge
    /// rather than a missing one.
    ModuleOnly,
    /// A glob, whose path ends in the `*` segment. Its source is a module or
    /// an enum — `use self::Kind::*` over an enum's variants is ordinary Rust
    /// — so the item table is probed, and a miss is a module that could not
    /// be placed rather than a name absent from a complete table.
    Glob,
    /// An ordinary path. A miss at its last segment means the container was
    /// there and the name was not.
    Path,
}

fn shape_of(target: &RefTarget) -> Shape {
    if target.segments.last().is_some_and(|s| s == "*") {
        return Shape::Glob;
    }
    if matches!(target.root, TargetRoot::This { .. })
        && target.segments.first().is_some_and(|s| s != "self")
    {
        return Shape::ModuleOnly;
    }
    Shape::Path
}

/// The inline module chain the site sits in, read back off the marked prefix
/// of its encloser's path.
///
/// The extractor writes that prefix from the same ancestor walk that fills a
/// relative path's qualifier, so the two cannot disagree; this is the channel
/// a non-relative path has, because [`TargetRoot::Name`] carries no qualifier
/// of its own.
fn site_chain(r: &Reference) -> Vec<String> {
    r.enclosing
        .iter()
        .flat_map(|e| e.path.iter())
        .take_while(|s| s.starts_with(MODULE_MARK))
        .map(|s| s[MODULE_MARK.len()..].to_string())
        .collect()
}

fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// Resolve one path against the graph, from its root to its last segment.
///
/// Shared by [`Resolver::resolve`] and [`Resolver::def_alias_targets`],
/// because a re-export's target and an import's target are the same question
/// asked in two phases — and two copies of this walk would drift, leaving an
/// alias pointing somewhere no import ever reaches.
fn place(
    cfg: &RsWorkspace,
    scope: &RsScope,
    target: &RefTarget,
    site_chain: &[String],
    probe: &dyn SymbolProbe,
) -> (Placed, Vec<NodeId>) {
    let mut candidates: Vec<NodeId> = Vec::new();
    let segments = &target.segments;
    let shape = shape_of(target);
    // A glob's `*` is not a name to look up: the path it globs is.
    let segments: &[String] = match segments.last() {
        Some(last) if last == "*" => &segments[..segments.len() - 1],
        _ => &segments[..],
    };

    let (base, rest): (String, &[String]) = match &target.root {
        TargetRoot::This { qualifier } => {
            let base = join_module(&scope.module, qualifier);
            let rest = match segments.first() {
                Some(first) if first == "self" => &segments[1..],
                _ => segments,
            };
            (base, rest)
        }
        TargetRoot::Super { qualifier } => {
            let mut base = join_module(&scope.module, qualifier);
            let hops = segments.iter().take_while(|s| *s == "super").count();
            for _ in 0..hops {
                match parent_module(&base) {
                    Some(parent) => base = parent.to_string(),
                    // `super` at a crate root names nothing. Inventing a
                    // parent would resolve a path rustc rejects.
                    None => {
                        return (
                            Placed::Missing(UnresolvedReason::ModuleNotFound),
                            candidates,
                        );
                    }
                }
            }
            (base, &segments[hops..])
        }
        // Uniform paths: since the 2018 edition a `use` path may also begin at
        // an item of the module it is written in, and one reference in the
        // measured corpus does — `use FastMatchResult::*` over an enum beside
        // it. The local reading is tried *first*, because a crate name reaches
        // a `use` path through the extern prelude and a prelude loses to a
        // declaration written in the module: `mod serde;` beside a registry
        // `serde` dependency binds the module, and so does `mod lib_one;`
        // beside a `path = "…"` dependency keyed `lib_one`. Taking the
        // dependency there would be a *wrong* edge counted `Resolved`, which
        // is worse than a miss.
        //
        // It is kept only when it *lands*: a first segment that is neither
        // anything in this module nor a declared crate is an unknown package,
        // and saying so is more useful than saying a name is absent.
        TargetRoot::Name => match segments.first() {
            Some(first) if first == "crate" => (scope.crate_root.clone(), &segments[1..]),
            Some(first) => {
                let local = join_module(&scope.module, site_chain);
                let (uniform, probed) = walk(&local, segments, shape, probe, &mut candidates);
                candidates = probed;
                if let Placed::Node(fqn) = uniform {
                    return (Placed::Node(fqn), candidates);
                }
                match crate_base(cfg, scope, first) {
                    Ok(root) => (root, &segments[1..]),
                    Err(placed) => return (placed, candidates),
                }
            }
            None => {
                return (
                    Placed::Missing(UnresolvedReason::ModuleNotFound),
                    candidates,
                );
            }
        },
        // Unreachable at tier 2: no expression-level reference is emitted, so
        // no target has an expression at its root. Answered rather than
        // asserted, because the resolver never drops.
        TargetRoot::Expr => {
            return (
                Placed::Missing(UnresolvedReason::NeedsExpressionType),
                candidates,
            );
        }
    };

    walk(&base, rest, shape, probe, &mut candidates)
}

/// The module a non-relative path's first segment roots at.
///
/// `Err` when the path leaves the repository, or names a crate no manifest
/// declares.
fn crate_base(cfg: &RsWorkspace, scope: &RsScope, name: &str) -> Result<String, Placed> {
    if SYSROOT_CRATES.contains(&name) {
        return Err(Placed::External(name.to_string()));
    }
    let package = scope.package.ok_or(Placed::Missing(
        // No manifest governs the file, so nothing here can say what its
        // dependencies are. That is arthron's own inference falling short,
        // not a name that is absent.
        UnresolvedReason::ProjectLayoutUnknown,
    ))?;
    match cfg.packages[package].deps.get(name) {
        Some(Dep::Local(dir)) => cfg
            .package_at(dir)
            .and_then(|p| cfg.lib_root(p))
            .map(str::to_string)
            // A `path = …` dependency whose library this scan never walked:
            // the crate is in the repository and its module tree is not.
            .ok_or(Placed::Missing(UnresolvedReason::ModuleNotFound)),
        Some(Dep::External) => Err(Placed::External(name.to_string())),
        None => {
            // A test, example, bench or binary target links its own package's
            // library and names it by the package's name.
            let own = cfg.packages[package].name.replace('-', "_");
            if !scope.in_lib
                && own == name
                && let Some(root) = cfg.lib_root(package)
            {
                return Ok(root.to_string());
            }
            Err(Placed::Missing(UnresolvedReason::UnknownPackage))
        }
    }
}

/// Walk the segments below a base module.
fn walk(
    base: &str,
    rest: &[String],
    shape: Shape,
    probe: &dyn SymbolProbe,
    candidates: &mut Vec<NodeId>,
) -> (Placed, Vec<NodeId>) {
    let mut current = base.to_string();
    candidates.push(node_id(Domain::Rust, &current));
    let mut known = probe.probe(&node_id(Domain::Rust, &current)).is_some();
    // A glob over the module the site already sits in, or a bare
    // `use somecrate;` — the base is the whole answer.
    if rest.is_empty() {
        let placed = if known {
            Placed::Node(current)
        } else {
            Placed::Missing(UnresolvedReason::ModuleNotFound)
        };
        return (placed, std::mem::take(candidates));
    }

    for (i, segment) in rest.iter().enumerate() {
        let last = i + 1 == rest.len();
        let child = format!("{current}::{segment}");
        let child_id = node_id(Domain::Rust, &child);
        candidates.push(child_id);
        if probe.probe(&child_id).is_some() {
            current = child;
            known = true;
            continue;
        }
        // Not a module. A `mod` declaration can be nothing else, and probing
        // the item table for one would let it resolve to a function.
        if shape == Shape::ModuleOnly {
            return (
                Placed::Missing(UnresolvedReason::ModuleNotFound),
                std::mem::take(candidates),
            );
        }
        let item = format!("{current}#{segment}");
        let item_id = node_id(Domain::Rust, &item);
        candidates.push(item_id);
        let Some(entry) = probe.probe(&item_id) else {
            let reason = if last && known && shape == Shape::Path {
                UnresolvedReason::NoMatchingDefinition
            } else {
                // A prefix segment that is neither a module nor an item, a
                // glob whose source could not be placed, or a container this
                // scan never saw: in all three the path stopped short of a
                // module rather than reaching a complete table without the
                // name in it.
                UnresolvedReason::ModuleNotFound
            };
            return (Placed::Missing(reason), std::mem::take(candidates));
        };
        if last {
            return (Placed::Node(item), std::mem::take(candidates));
        }
        // Segments below an item are its members: an enum's variants, a
        // trait's associated items. An alias is where the walk stops — a
        // stored forward carries an identity rather than a name, so there is
        // nothing to compose the next segment onto, and a cold scan has not
        // even placed the forward yet. Both shapes stop here, so the reason
        // is the same on a cold store and a warm one.
        //
        // This track's alias-hop ceiling is zero, for the reason the module
        // docs give, so the reason below is `AliasCycle`'s "ran past the hop
        // ceiling" clause and never its loop one: no chain is walked, so no
        // chain can re-enter itself.
        let is_alias = matches!(entry, Entry::Alias { .. })
            || matches!(
                entry,
                Entry::Definition {
                    kind: DefKind::Alias,
                    ..
                }
            );
        if is_alias {
            return (
                Placed::Missing(UnresolvedReason::AliasCycle),
                std::mem::take(candidates),
            );
        }
        let mut member = item;
        for segment in &rest[i + 1..] {
            member = format!("{member}.{segment}");
            let member_id = node_id(Domain::Rust, &member);
            candidates.push(member_id);
            if probe.probe(&member_id).is_none() {
                return (
                    Placed::Missing(UnresolvedReason::NoMatchingDefinition),
                    std::mem::take(candidates),
                );
            }
        }
        return (Placed::Node(member), std::mem::take(candidates));
    }
    let placed = if known {
        Placed::Node(current)
    } else {
        Placed::Missing(UnresolvedReason::ModuleNotFound)
    };
    (placed, std::mem::take(candidates))
}

impl Resolver<RsLang> for RsResolver {
    fn config(&self, root: &Path, files: &FileIndex) -> Result<RsWorkspace, LayoutError> {
        Ok(RsWorkspace::load(root, &files.files))
    }

    fn config_digest(&self, cfg: &RsWorkspace) -> Vec<u8> {
        cfg.digest()
    }

    fn declared_container(
        &self,
        _cfg: &RsWorkspace,
        _header: &RsHeader,
    ) -> Option<(String, String)> {
        // A Rust module's name is its file's, and its place is its path
        // relative to a crate root the manifest names. Both are per-file
        // derivable, so no container name has to travel between files.
        None
    }

    fn learn_containers(&self, _cfg: &mut RsWorkspace, _names: &HashMap<String, String>) {}

    fn owns_file(&self, _cfg: &RsWorkspace, _rel_path: &str) -> bool {
        // Every `.rs` file the walk reached belongs to this scan. A nested
        // manifest inside a workspace is a *member*, not a separate project
        // the way a nested `go.mod` is.
        true
    }

    fn def_fqn(
        &self,
        cfg: &RsWorkspace,
        header: &RsHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        let file_module = cfg.module_fqn(&header.rel_path);
        // Module segments are marked and come first; everything after them is
        // the type chain. See `track_rust::extract` for why the split is
        // carried this way and not in a span.
        let depth = owner
            .iter()
            .take_while(|s| s.starts_with(MODULE_MARK))
            .count();
        let mods: Vec<String> = owner[..depth]
            .iter()
            .map(|s| s[MODULE_MARK.len()..].to_string())
            .collect();
        let module = join_module(&file_module, &mods);
        let name = def.name.strip_prefix(MODULE_MARK).unwrap_or(&def.name);
        if def.kind == DefKind::Module {
            // The file's own module: synthesized here rather than written in
            // this file, and named by the manifest and the path together.
            if def.facets.contains(crate::model::DefFacets::SYNTHETIC) && owner.is_empty() {
                return Some(Fqn::new(file_module));
            }
            return Some(Fqn::new(format!("{module}::{name}")));
        }
        // `#` separates a container from its members and `.` joins a member
        // chain inside one. Neither can appear in a Rust identifier, and a
        // crate root is a file path, which carries no `::` — so a module FQN
        // and an item FQN can never collide, and `mod foo;` beside `fn foo()`
        // stays two nodes exactly as Rust's two namespaces require.
        let types = &owner[depth..];
        if types.is_empty() {
            Some(Fqn::new(format!("{module}#{name}")))
        } else {
            Some(Fqn::new(format!("{module}#{}.{name}", types.join("."))))
        }
    }

    fn def_alias_targets(
        &self,
        cfg: &RsWorkspace,
        header: &RsHeader,
        def: &Definition,
        probe: &dyn SymbolProbe,
    ) -> Vec<Fqn> {
        if def.kind != DefKind::Alias {
            return Vec::new();
        }
        // Paired by byte offset: this runs in the definition phase, where a
        // definition's span is the real one.
        let Some(export) = header
            .reexports
            .iter()
            .find(|e| e.byte_start == def.span.byte_start)
        else {
            return Vec::new();
        };
        let scope = RsScope::of(cfg, &header.rel_path);
        match place(cfg, &scope, &export.target, &export.module, probe).0 {
            Placed::Node(fqn) => vec![Fqn::new(fqn)],
            // A re-export of something outside the repository, or of a name
            // this scan cannot place, is still an alias — it just forwards
            // nowhere arthron can name, and an empty target says exactly that.
            _ => Vec::new(),
        }
    }

    fn index_keys(&self, _cfg: &RsWorkspace, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // A Rust definition is reached by its path alone: no overload sets and
        // no member-name keys.
        Vec::new()
    }

    fn mergeable(&self, _a: &Definition, _b: &Definition) -> bool {
        // Two declarations sharing an FQN are two entities, never one:
        // `#[cfg(unix)] fn f()` and `#[cfg(windows)] fn f()` are both real,
        // and merging them would hide that the graph holds a union over
        // configurations.
        false
    }

    fn scope(
        &self,
        cfg: &RsWorkspace,
        file: &FileFacts<RsLang>,
        _probe: &dyn SymbolProbe,
    ) -> RsScope {
        RsScope::of(cfg, &file.header.rel_path)
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // Tier 2 emits no `Inherit` reference: a trait bound is a type use,
        // and type-use resolution is exactly what this tier does not claim.
        &[]
    }

    fn resolve(
        &self,
        cfg: &RsWorkspace,
        scope: &RsScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        if r.kind != RefKind::Import {
            // Unreachable while the extractor emits imports alone. Answered
            // rather than asserted: the resolver never drops, and a future
            // reference kind must arrive with a rule rather than a panic.
            return unresolved(UnresolvedReason::TierTwoLanguage);
        }
        let (placed, candidates) = place(cfg, scope, &r.target, &site_chain(r), probe);
        let outcome = match placed {
            Placed::Node(fqn) => Outcome::Resolved(node_id(Domain::Rust, &fqn)),
            Placed::External(krate) => Outcome::External(krate),
            Placed::Missing(reason) => Outcome::Unresolved(reason),
        };
        Resolution {
            outcome,
            candidates,
        }
    }
}

/// The Rust track's scan entry point, reading every `.rs` the walk finds.
pub fn scan_rust(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_rust_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_rust`] under a repository's include/exclude globs. What
/// [`crate::track_rust::TRACK`] holds.
pub fn scan_rust_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<RsLang>(
        root,
        db,
        &crate::track_rust::extract::RsExtractor,
        &RsResolver,
        filter,
    )
}

/// Rust's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(RsLang::LANG, Lang::Rust));
    assert!(matches!(RsLang::DOMAIN, Domain::Rust));
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DefFacets;
    use crate::track_rust::extract::extract;
    use std::collections::HashSet;

    /// A one-package workspace whose library is at `src/lib.rs`.
    fn lib_only() -> RsWorkspace {
        use crate::track_rust::project::{Package, Target};
        RsWorkspace {
            packages: vec![Package {
                dir: String::new(),
                name: "demo".into(),
                deps: [
                    ("serde".to_string(), Dep::External),
                    ("sibling".to_string(), Dep::Local("crates/sibling".into())),
                ]
                .into_iter()
                .collect(),
            }],
            targets: vec![Target {
                root: "src/lib.rs".into(),
                package: 0,
                kind: TargetKind::Lib,
            }],
        }
    }

    fn table(fqns: &[&str]) -> HashSet<NodeId> {
        fqns.iter().map(|f| node_id(Domain::Rust, f)).collect()
    }

    fn outcomes(
        cfg: &RsWorkspace,
        rel_path: &str,
        source: &str,
        known: &[&str],
    ) -> Vec<(String, Outcome<NodeId, String>)> {
        let known = table(known);
        let facts = extract(rel_path, source);
        let scope = RsResolver.scope(cfg, &facts, &known);
        facts
            .refs
            .iter()
            .map(|r| {
                (
                    r.raw_target.clone(),
                    RsResolver.resolve(cfg, &scope, r, &known).outcome,
                )
            })
            .collect()
    }

    fn outcome_of(
        cfg: &RsWorkspace,
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

    fn resolved(fqn: &str) -> Outcome<NodeId, String> {
        Outcome::Resolved(node_id(Domain::Rust, fqn))
    }

    /// A two-package workspace: the root's library, plus a `path = …` sibling
    /// keyed `sibling`.
    fn with_sibling() -> RsWorkspace {
        use crate::track_rust::project::{Package, Target};
        RsWorkspace {
            packages: vec![
                Package {
                    dir: String::new(),
                    name: "app".into(),
                    deps: [("sibling".to_string(), Dep::Local("crates/sibling".into()))]
                        .into_iter()
                        .collect(),
                },
                Package {
                    dir: "crates/sibling".into(),
                    name: "sibling".into(),
                    deps: Default::default(),
                },
            ],
            targets: vec![
                Target {
                    root: "crates/sibling/src/lib.rs".into(),
                    package: 1,
                    kind: TargetKind::Lib,
                },
                Target {
                    root: "src/lib.rs".into(),
                    package: 0,
                    kind: TargetKind::Lib,
                },
            ],
        }
    }

    #[test]
    fn a_path_dependency_is_what_its_name_reaches_when_nothing_local_answers() {
        assert_eq!(
            outcome_of(
                &with_sibling(),
                "src/lib.rs",
                "use sibling::Thing;\n",
                &[
                    "crates/sibling/src/lib.rs",
                    "crates/sibling/src/lib.rs#Thing"
                ],
                "sibling::Thing",
            ),
            resolved("crates/sibling/src/lib.rs#Thing"),
        );
    }

    #[test]
    fn a_module_written_beside_a_path_dependency_of_the_same_name_wins() {
        // A crate name reaches a `use` path through the extern prelude, and a
        // prelude loses to a declaration written in the module: rustc binds
        // `mod sibling;`, and both readings are in the table here, so the
        // resolver has to choose the same one. Taking the dependency would be
        // a wrong edge counted `Resolved`, which reads as success.
        assert_eq!(
            outcome_of(
                &with_sibling(),
                "src/lib.rs",
                "mod sibling;\nuse sibling::Thing;\n",
                &[
                    "src/lib.rs",
                    "src/lib.rs::sibling",
                    "src/lib.rs::sibling#Thing",
                    "crates/sibling/src/lib.rs",
                    "crates/sibling/src/lib.rs#Thing",
                ],
                "sibling::Thing",
            ),
            resolved("src/lib.rs::sibling#Thing"),
        );
    }

    #[test]
    fn a_crate_path_roots_at_the_targets_root_file() {
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/a/b.rs",
                "use crate::a::b::Thing;\n",
                &[
                    "src/lib.rs",
                    "src/lib.rs::a",
                    "src/lib.rs::a::b",
                    "src/lib.rs::a::b#Thing",
                ],
                "crate::a::b::Thing",
            ),
            resolved("src/lib.rs::a::b#Thing"),
        );
    }

    #[test]
    fn self_is_the_module_the_site_sits_in_inline_blocks_included() {
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/a.rs",
                "mod inner { use self::deep::T; }\n",
                &["src/lib.rs::a::inner::deep", "src/lib.rs::a::inner::deep#T"],
                "self::deep::T",
            ),
            resolved("src/lib.rs::a::inner::deep#T"),
        );
    }

    #[test]
    fn super_climbs_one_module_per_keyword_and_stops_at_the_crate_root() {
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/a/b.rs",
                "use super::super::Top;\n",
                &["src/lib.rs#Top"],
                "super::super::Top",
            ),
            resolved("src/lib.rs#Top"),
        );
        // One `super` too many names nothing above the crate root.
        assert_eq!(
            outcome_of(
                &cfg,
                "src/a/b.rs",
                "use super::super::super::Top;\n",
                &["src/lib.rs#Top"],
                "super::super::super::Top",
            ),
            Outcome::Unresolved(UnresolvedReason::ModuleNotFound),
        );
    }

    #[test]
    fn the_sysroot_and_a_declared_dependency_leave_the_repository() {
        let cfg = lib_only();
        for (source, raw, krate) in [
            ("use std::io::Write;\n", "std::io::Write", "std"),
            ("use core::fmt;\n", "core::fmt", "core"),
            ("use serde::Serialize;\n", "serde::Serialize", "serde"),
        ] {
            assert_eq!(
                outcome_of(&cfg, "src/lib.rs", source, &[], raw),
                Outcome::External(krate.to_string()),
                "{raw}",
            );
        }
    }

    #[test]
    fn an_undeclared_crate_is_an_unknown_package_and_not_a_missing_name() {
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/lib.rs",
                "use nowhere::Thing;\n",
                &[],
                "nowhere::Thing",
            ),
            Outcome::Unresolved(UnresolvedReason::UnknownPackage),
        );
    }

    #[test]
    fn a_path_dependency_roots_at_the_sibling_crates_library() {
        use crate::track_rust::project::{Package, Target};
        let mut cfg = lib_only();
        cfg.packages.push(Package {
            dir: "crates/sibling".into(),
            name: "sibling".into(),
            deps: Default::default(),
        });
        cfg.targets.push(Target {
            root: "crates/sibling/src/lib.rs".into(),
            package: 1,
            kind: TargetKind::Lib,
        });
        assert_eq!(
            outcome_of(
                &cfg,
                "src/lib.rs",
                "use sibling::Thing;\n",
                &["crates/sibling/src/lib.rs#Thing"],
                "sibling::Thing",
            ),
            resolved("crates/sibling/src/lib.rs#Thing"),
        );
    }

    #[test]
    fn a_test_target_names_its_own_packages_library_by_the_package_name() {
        use crate::track_rust::project::Target;
        let mut cfg = lib_only();
        cfg.targets.push(Target {
            root: "tests/it.rs".into(),
            package: 0,
            kind: TargetKind::Test,
        });
        assert_eq!(
            outcome_of(
                &cfg,
                "tests/it.rs",
                "use demo::Thing;\n",
                &["src/lib.rs#Thing"],
                "demo::Thing",
            ),
            resolved("src/lib.rs#Thing"),
        );
        // The library cannot name itself that way, and nothing pretends it can.
        assert_eq!(
            outcome_of(
                &cfg,
                "src/lib.rs",
                "use demo::Thing;\n",
                &["src/lib.rs#Thing"],
                "demo::Thing",
            ),
            Outcome::Unresolved(UnresolvedReason::UnknownPackage),
        );
    }

    #[test]
    fn a_module_declaration_names_the_module_its_own_file_declares() {
        let cfg = lib_only();
        assert_eq!(
            outcome_of(&cfg, "src/lib.rs", "mod a;\n", &["src/lib.rs::a"], "mod a"),
            resolved("src/lib.rs::a"),
        );
        // No file, so no module. `ModuleNotFound`, not "our bug".
        assert_eq!(
            outcome_of(&cfg, "src/lib.rs", "mod a;\n", &[], "mod a"),
            Outcome::Unresolved(UnresolvedReason::ModuleNotFound),
        );
    }

    #[test]
    fn a_glob_names_the_module_it_globs() {
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/a.rs",
                "use super::b::*;\n",
                &["src/lib.rs::b"],
                "super::b::*",
            ),
            resolved("src/lib.rs::b"),
        );
        assert_eq!(
            outcome_of(
                &cfg,
                "src/a.rs",
                "use super::*;\n",
                &["src/lib.rs"],
                "super::*"
            ),
            resolved("src/lib.rs"),
        );
    }

    #[test]
    fn a_glob_over_an_enum_names_the_enum() {
        // `use self::Kind::*;` over an enum's variants is ordinary Rust, and
        // the corpus writes it 29 times. A glob's source is a module *or* an
        // item, so both tables are read.
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/a.rs",
                "use self::Kind::*;\n",
                &["src/lib.rs::a", "src/lib.rs::a#Kind"],
                "self::Kind::*",
            ),
            resolved("src/lib.rs::a#Kind"),
        );
    }

    #[test]
    fn a_uniform_path_begins_at_the_module_the_use_is_written_in() {
        // Since the 2018 edition a `use` path may begin at an item of its own
        // module. Tried only after every dependency table has missed.
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/a.rs",
                "fn f() { use Kind::*; }\n",
                &["src/lib.rs::a", "src/lib.rs::a#Kind"],
                "Kind::*",
            ),
            resolved("src/lib.rs::a#Kind"),
        );
        // And when it lands nowhere, the answer is still the one the
        // dependency tables gave: this names a crate nobody declared.
        assert_eq!(
            outcome_of(
                &cfg,
                "src/a.rs",
                "use Kind::*;\n",
                &["src/lib.rs::a"],
                "Kind::*"
            ),
            Outcome::Unresolved(UnresolvedReason::UnknownPackage),
        );
    }

    #[test]
    fn a_module_declaration_never_resolves_to_a_function_of_the_same_name() {
        // `mod a;` beside `fn a()` must miss: probing the item table for a
        // module declaration is a wrong edge, not a recovered one.
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/lib.rs",
                "mod a;\n",
                &["src/lib.rs", "src/lib.rs#a"],
                "mod a",
            ),
            Outcome::Unresolved(UnresolvedReason::ModuleNotFound),
        );
    }

    #[test]
    fn a_module_wins_over_an_item_of_the_same_name() {
        // `mod foo;` beside `fn foo()` is legal Rust — two namespaces — and
        // `use crate::foo::Thing` reads the module.
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/lib.rs",
                "use crate::foo::Thing;\n",
                &["src/lib.rs::foo", "src/lib.rs#foo", "src/lib.rs::foo#Thing"],
                "crate::foo::Thing",
            ),
            resolved("src/lib.rs::foo#Thing"),
        );
    }

    #[test]
    fn a_variant_is_reached_through_its_enum() {
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/lib.rs",
                "use crate::m::E::A;\n",
                &["src/lib.rs::m", "src/lib.rs::m#E", "src/lib.rs::m#E.A"],
                "crate::m::E::A",
            ),
            resolved("src/lib.rs::m#E.A"),
        );
    }

    #[test]
    fn a_name_that_is_not_there_is_a_missing_definition_not_a_missing_module() {
        let cfg = lib_only();
        assert_eq!(
            outcome_of(
                &cfg,
                "src/lib.rs",
                "use crate::m::Absent;\n",
                &["src/lib.rs::m"],
                "crate::m::Absent",
            ),
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
        );
        // A prefix that names no module is the other failure, and it says so.
        assert_eq!(
            outcome_of(
                &cfg,
                "src/lib.rs",
                "use crate::gone::Thing;\n",
                &[],
                "crate::gone::Thing",
            ),
            Outcome::Unresolved(UnresolvedReason::ModuleNotFound),
        );
    }

    #[test]
    fn a_re_export_is_a_declaration_site_a_later_use_can_reach() {
        let cfg = lib_only();
        let facts = extract("src/lib.rs", "pub use crate::m::Thing;\n");
        let alias = facts
            .defs
            .iter()
            .find(|d| d.kind == DefKind::Alias)
            .expect("a public re-export binds an alias");
        assert_eq!(
            RsResolver
                .def_fqn(&cfg, &facts.header, &alias.owner, alias, &table(&[]))
                .map(Fqn::into_string),
            Some("src/lib.rs#Thing".to_string()),
        );
        // And it forwards to the definition it re-exports.
        let known = table(&["src/lib.rs::m", "src/lib.rs::m#Thing"]);
        assert_eq!(
            RsResolver
                .def_alias_targets(&cfg, &facts.header, alias, &known)
                .into_iter()
                .map(Fqn::into_string)
                .collect::<Vec<_>>(),
            ["src/lib.rs::m#Thing"],
        );
    }

    #[test]
    fn an_alias_is_where_a_walk_stops_rather_than_a_chain_it_invents() {
        let cfg = lib_only();
        struct Aliased(NodeId);
        impl SymbolProbe for Aliased {
            fn probe(&self, id: &NodeId) -> Option<Entry> {
                (*id == self.0).then_some(Entry::Alias {
                    target: node_id(Domain::Rust, "elsewhere"),
                })
            }
        }
        let probe = Aliased(node_id(Domain::Rust, "src/lib.rs#re"));
        let facts = extract("src/lib.rs", "use crate::re::Deeper;\n");
        let scope = RsResolver.scope(&cfg, &facts, &probe);
        assert_eq!(
            RsResolver
                .resolve(&cfg, &scope, &facts.refs[0], &probe)
                .outcome,
            Outcome::Unresolved(UnresolvedReason::AliasCycle),
        );
    }

    #[test]
    fn every_probe_is_recorded_hit_and_miss_alike() {
        let cfg = lib_only();
        let known = table(&["src/lib.rs::m", "src/lib.rs::m#Thing"]);
        let facts = extract("src/lib.rs", "use crate::m::Thing;\n");
        let scope = RsResolver.scope(&cfg, &facts, &known);
        let res = RsResolver.resolve(&cfg, &scope, &facts.refs[0], &known);
        // The base, the module `m`, the child module `m::Thing` that is not
        // there, and the item that is. The miss is recorded too: it is what
        // makes an edit that *creates* `crate::m::Thing` as a module wake this
        // very reference.
        assert_eq!(
            res.candidates,
            vec![
                node_id(Domain::Rust, "src/lib.rs"),
                node_id(Domain::Rust, "src/lib.rs::m"),
                node_id(Domain::Rust, "src/lib.rs::m::Thing"),
                node_id(Domain::Rust, "src/lib.rs::m#Thing"),
            ],
        );
    }

    #[test]
    fn the_files_own_module_is_named_by_the_manifest_and_the_path() {
        let cfg = lib_only();
        for (path, want) in [
            ("src/lib.rs", "src/lib.rs"),
            ("src/a.rs", "src/lib.rs::a"),
            ("src/a/mod.rs", "src/lib.rs::a"),
            ("src/a/b.rs", "src/lib.rs::a::b"),
        ] {
            let facts = extract(path, "");
            let module = &facts.defs[0];
            assert!(module.facets.contains(DefFacets::SYNTHETIC));
            assert_eq!(
                RsResolver
                    .def_fqn(&cfg, &facts.header, &module.owner, module, &table(&[]))
                    .map(Fqn::into_string),
                Some(want.to_string()),
                "{path}",
            );
        }
    }

    #[test]
    fn an_inline_module_nests_and_a_type_does_not() {
        let cfg = lib_only();
        let facts = extract(
            "src/a.rs",
            "mod t { pub struct S; impl S { fn m(&self) {} } }\n",
        );
        let fqn = |name: &str| {
            let def = facts
                .defs
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("no definition `{name}`"));
            RsResolver
                .def_fqn(&cfg, &facts.header, &def.owner, def, &table(&[]))
                .map(Fqn::into_string)
                .unwrap()
        };
        assert_eq!(fqn("t"), "src/lib.rs::a::t");
        assert_eq!(fqn("S"), "src/lib.rs::a::t#S");
        assert_eq!(fqn("m"), "src/lib.rs::a::t#S.m");
    }

    #[test]
    fn the_marker_never_reaches_an_fqn() {
        let cfg = lib_only();
        let facts = extract("src/a.rs", "mod t { pub fn f() {} use crate::x::Y; }\n");
        for def in &facts.defs {
            let fqn = RsResolver
                .def_fqn(&cfg, &facts.header, &def.owner, def, &table(&[]))
                .map(Fqn::into_string)
                .unwrap();
            assert!(!fqn.contains(":::"), "{fqn}");
        }
        // And the encloser the driver sources the import at is the module.
        let encloser = facts.refs[0].enclosing.as_ref().unwrap();
        let def = encloser.as_definition().unwrap();
        assert_eq!(
            RsResolver
                .def_fqn(&cfg, &facts.header, &def.owner, &def, &table(&[]))
                .map(Fqn::into_string),
            Some("src/lib.rs::a::t".to_string()),
        );
    }

    #[test]
    fn nothing_this_track_emits_is_ever_a_local_binding() {
        // The tier-2 contract: no expression-level reference exists for a
        // block to bind, so the reason is unreachable rather than rare.
        let cfg = lib_only();
        let out = outcomes(
            &cfg,
            "src/lib.rs",
            "fn f() { use std::io; let io = 1; }\n",
            &[],
        );
        assert!(!out.is_empty());
        for (raw, outcome) in out {
            assert_ne!(
                outcome,
                Outcome::Unresolved(UnresolvedReason::LocalBinding),
                "{raw}",
            );
        }
    }
}
