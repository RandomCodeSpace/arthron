//! The one place a Haskell [`crate::Outcome`] is produced. Never drops.
//!
//! # The gate is an import-resolution rate
//!
//! Haskell is **tier 2, best effort** here: definitions, structure and
//! imports, and no verified call edges. The references this resolver
//! classifies are exactly the `import` declarations the extractor emits, and
//! the rate the gate defends is the share of them that name a module this
//! repository declares. Every one ends `Resolved`, `External`, or
//! `Unresolved` with a reason.
//!
//! # The import model, as measured
//!
//! `import M` names a module, and GHC finds it by looking for
//! `<root>/M/with/slashes.hs` under each `hs-source-dirs` root of the
//! component being compiled. So resolution is one rule with one input phase 0
//! supplies:
//!
//! 1. **Turn the dotted name into a path** — `Data.Aeson.Key` →
//!    `Data/Aeson/Key.hs` — and probe it under each source root in turn.
//! 2. **The importing file's own root goes first.** That is GHC's own order,
//!    a home module is looked for in its own component's source tree before
//!    anywhere else, and the measured corpus turns on it: two files in aeson
//!    are git symlinks sharing one blob between `src/Data/Aeson/Internal/`
//!    and `attoparsec-aeson/src/Data/Aeson/Internal/`, so
//!    `Data.Aeson.Internal.ByteString` exists under two roots. Own-root-first
//!    binds each package's import to that package's own copy — the same file
//!    either way, and the right answer for the right reason.
//!
//! `qualified`, `as` and `hiding` never reach this layer. All three change
//! what the importing file may write afterwards; none changes what is named.
//!
//! # What each miss means, and why `External` is a fact here
//!
//! `External` sits outside **both** terms of the resolution rate, so widening
//! it is the cheapest way there is to raise a rate with nothing linked. Three
//! guards stand in front of it, and each is a fixture in this module:
//!
//! - **No source root at all** ⇒ [`crate::UnresolvedReason::ProjectLayoutUnknown`]
//!   for every import, and never `External`. Without a `.cabal` nothing in the
//!   tree is a home module, so an unguarded rule would classify the *entire*
//!   denominator as external and report a rate of nothing at all. Phase 0
//!   failing must cost the measurement, not empty it.
//! - **The name is a module this repository declares, and no configured root
//!   explains its path** ⇒ [`crate::UnresolvedReason::ProjectLayoutUnknown`],
//!   and never `External`. This is the anti-laundering guard proper: the
//!   driver hands over every module name the walk found (see
//!   [`crate::lang::Resolver::learn_containers`]), so a root map that missed a
//!   component is reported as *this build's* layout failure, inside the
//!   denominator, instead of vanishing outside both rate terms. The measured
//!   corpus produces none of these, which is the point of pinning it at zero.
//! - **The repository declares no dependency on a package it does not
//!   contain** ⇒ [`crate::UnresolvedReason::UnknownPackage`]. `External` is a
//!   claim about the outside world, and the only evidence for it available
//!   without a compiler is the repository's own `build-depends` naming
//!   packages its manifests do not declare. A tree that names none has not
//!   made that claim on this resolver's behalf. Read it for what it is: one
//!   boolean about the whole repository, not a per-name check. Every real
//!   project depends on `base`, so in practice it is true and the guard above
//!   plus the `external` count the gate pins are what actually bite. A
//!   per-name check would need a package database, for the reason the next
//!   section gives.
//!
//! Past all three, an import naming no home module **is** external, and that
//! is a fact rather than an inference: a home module is exactly a file under a
//! declared root, this walk enumerates both, and a name that is under no root
//! cannot be in this repository at all.
//!
//! # What the external node's *name* costs
//!
//! Precision, not honesty. A Haskell module name states no boundary between
//! the package and the module — `bytestring` ships `Data.ByteString`,
//! `containers` ships `Data.Map`, `text` ships `Data.Text` — and a `.cabal`
//! `build-depends` list names packages without saying which modules they
//! expose. Nothing short of a package database closes that gap, and this build
//! reads none and makes no network call. So the node is named by the module's
//! **root segment**: `Data`, `Test`, `Control`, `GHC`. Sixteen such nodes in
//! the measured corpus for some thirty real packages. That is the coarsest
//! unit nameable without guessing where a dependency's namespace begins, which
//! is the answer the Kotlin and PHP tracks reached for the same reason, and it
//! moves no reference between the rate's terms.
//!
//! # `LocalBinding` does not apply here
//!
//! Tier 2 emits no expression-level reference, so no Haskell reference can
//! name a parameter, a local or a `where` binding. The bucket stays empty and
//! the baseline records it as zero — which makes this track's rate un-gameable
//! by the one reclassification the rate's own definition permits.
//!
//! # Known non-claims
//!
//! - **Cross-package visibility is not enforced.** GHC lets a component import
//!   a module from another package only when its `build-depends` names that
//!   package; this resolver probes every root in the repository, own root
//!   first. The shortcut can only ever *resolve* a reference, never launder
//!   one outside the denominator — but a resolution cabal would refuse is a
//!   wrong edge, so it is measured rather than argued: `tests/haskell_corpus.rs`
//!   walks every resolved row against a per-root visibility table read off the
//!   five manifests, and all 278 of the corpus's edges are ones cabal would
//!   also draw.
//! - **`{-# SOURCE #-}` imports and `.hs-boot` files are unexercised.** The
//!   corpus contains neither, and `.hs-boot` is an unclaimed extension.
//! - **A build-generated module is unexercised.** Cabal's `Paths_<pkg>` and
//!   the output of `hsc2hs`/`alex`/`happy` are home modules that exist only
//!   after a build; the corpus imports none, and one would land in `External`
//!   here rather than in [`crate::UnresolvedReason::Generated`].

use std::collections::HashMap;
use std::path::Path;

use crate::UnresolvedReason;
use crate::lang::{FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe};
use crate::model::{
    DefFacets, DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, Reference, node_id,
};
use crate::track_haskell::extract::{HsExtractor, HsHeader};
use crate::track_haskell::lang::{HsLang, MEMBER, module_fqn, module_path};
use crate::track_haskell::project::{HsProject, layout};

/// One file's view of where its own imports are looked up first.
///
/// One fact and no more: the source root this file itself sits under. What
/// each import *names* is on the reference — this track's `raw_target` is the
/// module name verbatim, with no alias and no selector list — so there is
/// nothing to pair and no unpaired case to invent a reason for.
pub struct HsScope {
    /// The longest declared source root this file sits under, if any.
    own_root: Option<String>,
}

/// An outcome with nothing probed.
fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: crate::Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// Whether a repo-relative path sits under a source root.
///
/// `""` is the repository root and holds everything; any other root must match
/// a whole path segment, so `src` does not claim `srcfoo/A.hs`.
fn under(root: &str, rel_path: &str) -> bool {
    root.is_empty()
        || rel_path
            .strip_prefix(root)
            .is_some_and(|r| r.starts_with('/'))
}

/// The identity of the module a repo-relative path (without `.hs`) is.
fn module_id(path: &str) -> NodeId {
    node_id(Domain::Haskell, &module_fqn(path))
}

/// The segment a dotted module name opens with: `Data.ByteString` → `Data`.
fn root_segment(module_name: &str) -> &str {
    module_name.split('.').next().unwrap_or(module_name)
}

/// Haskell's resolver. Stateless: everything it reads is in the config, the
/// scope, or the probe.
pub struct HsResolver;

impl HsResolver {
    /// The source roots this file's imports are probed under, in order: its
    /// own first, then every other root the manifests declare.
    fn probe_order<'a>(cfg: &'a HsProject, scope: &'a HsScope) -> impl Iterator<Item = &'a str> {
        scope
            .own_root
            .as_deref()
            .into_iter()
            .chain(cfg.source_roots.iter().map(String::as_str))
    }
}

impl Resolver<HsLang> for HsResolver {
    fn config(&self, root: &Path, _files: &FileIndex) -> Result<HsProject, LayoutError> {
        layout(root)
    }

    fn config_digest(&self, cfg: &HsProject) -> Vec<u8> {
        // The source roots root every candidate an import builds, so a scan
        // under a different set describes a different project and cannot be
        // patched into this one file by file. The module names the driver
        // teaches afterwards are deliberately *not* in here — they change as
        // the scan learns rather than as the project does, and folding them in
        // would wipe the store on every scan.
        cfg.digest()
    }

    fn declared_container(&self, _cfg: &HsProject, header: &HsHeader) -> Option<(String, String)> {
        // Every `.hs` file decides one name: the module it declares. The
        // identity is the path, because that is what distinguishes the six
        // files in the measured corpus that all declare `module Main`; the
        // name is what an `import` spells, and the resolver needs the set of
        // them to tell a root-map failure from a genuine outsider.
        Some((
            module_fqn(&header.rel_path),
            header.declared_name().to_string(),
        ))
    }

    fn learn_containers(&self, cfg: &mut HsProject, names: &HashMap<String, String>) {
        // Extended, never replaced: the driver calls this once with what the
        // store already holds and once with what this event's own files
        // declare, and a resolver that took only the second would forget every
        // module an incremental scan did not touch.
        cfg.declared_modules.extend(names.values().cloned());
    }

    fn owns_file(&self, _cfg: &HsProject, _rel_path: &str) -> bool {
        // No nested-manifest fence. A `.cabal` in a subdirectory is another
        // component of the same repository, and its source roots join the
        // probe order rather than carving files out of the scan — which is
        // what makes aeson's five packages one measurement.
        true
    }

    fn def_fqn(
        &self,
        _cfg: &HsProject,
        header: &HsHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        let module = module_fqn(&header.rel_path);
        // The file's own module node: synthesized, at the top level, and a
        // module. Its identity is the path, because a declared name is not
        // unique — six files in the measured corpus declare `module Main`.
        if def.kind == DefKind::Module
            && def.facets.contains(DefFacets::SYNTHETIC)
            && owner.is_empty()
        {
            return Some(Fqn::new(module));
        }
        if def.name.is_empty() {
            return None;
        }
        let mut path: Vec<&str> = owner.iter().map(String::as_str).collect();
        path.push(&def.name);
        Some(Fqn::new(format!("{module}{MEMBER}{}", path.join("."))))
    }

    fn index_keys(&self, _cfg: &HsProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // Every Haskell node is reachable by exactly one identity: a module by
        // its path, a declaration by its module and its owner chain. An import
        // names a module and nothing else, so there is no second keyspace to
        // fill.
        Vec::new()
    }

    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        // A type signature and the equation under it are one declaration
        // written twice — `x :: Int` then `x = 1` — and so are the two halves
        // of a pattern synonym, and a record field two constructors of one
        // type both name. All of them agree on kind, name and owner. A type
        // and a data constructor sharing a word do *not* collide here at all:
        // the constructor is filed under the type, so they never reach one
        // identity.
        a.kind == b.kind && a.name == b.name && a.owner == b.owner
    }

    fn scope(
        &self,
        cfg: &HsProject,
        file: &FileFacts<HsLang>,
        _probe: &dyn SymbolProbe,
    ) -> HsScope {
        // The longest match, so a file under `text-iso8601/src` is not claimed
        // by a `""` root declared elsewhere in the tree.
        HsScope {
            own_root: cfg
                .source_roots
                .iter()
                .filter(|root| under(root, &file.header.rel_path))
                .max_by_key(|root| root.len())
                .cloned(),
        }
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // Tier 2 emits no `Inherit` reference: a `deriving` clause and an
        // `instance` head are part of a declaration's structure here and are
        // not resolved, so there is no supertype relation to build and nothing
        // for the driver to run a phase over.
        &[]
    }

    fn resolve(
        &self,
        cfg: &HsProject,
        scope: &HsScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let name = r.raw_target.as_str();
        // Guard 1: without a source root nothing in this tree is a home
        // module, so every import would look like an outsider. A phase-0 gap
        // costs the measurement; it never empties it.
        if cfg.source_roots.is_empty() {
            return unresolved(UnresolvedReason::ProjectLayoutUnknown);
        }
        let suffix = module_path(name);
        let mut candidates: Vec<NodeId> = Vec::new();
        for root in Self::probe_order(cfg, scope) {
            let path = if root.is_empty() {
                suffix.clone()
            } else {
                format!("{root}/{suffix}")
            };
            let id = module_id(&path);
            if candidates.contains(&id) {
                continue;
            }
            candidates.push(id);
            if probe.probe(&id).is_some() {
                return Resolution {
                    outcome: crate::Outcome::Resolved(id),
                    candidates,
                };
            }
        }
        // Guard 2: the walk found a file declaring this very module and no
        // configured root explains where it sits. That is this build's own
        // layout inference failing, and it counts against the rate — the one
        // thing it must never do is leave the denominator as `External`.
        if cfg.declared_modules.contains(name) {
            return Resolution {
                outcome: crate::Outcome::Unresolved(UnresolvedReason::ProjectLayoutUnknown),
                candidates,
            };
        }
        // Guard 3, then the fact: no file under any declared root is this
        // module, so it is not in this repository — and the manifests say the
        // repository links against packages it does not contain.
        let outcome = if cfg.declares_outside_dependency() {
            crate::Outcome::External(root_segment(name).to_string())
        } else {
            crate::Outcome::Unresolved(UnresolvedReason::UnknownPackage)
        };
        Resolution {
            outcome,
            candidates,
        }
    }
}

/// The Haskell track's scan entry point, reading every `.hs` the walk finds.
pub fn scan_haskell(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_haskell_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_haskell`] under a repository's include/exclude globs. What
/// [`crate::track_haskell::TRACK`] holds.
pub fn scan_haskell_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<HsLang>(root, db, &HsExtractor, &HsResolver, filter)
}

/// Haskell's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(HsLang::LANG, Lang::Haskell));
    assert!(matches!(HsLang::DOMAIN, Domain::Haskell));
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Outcome;
    use crate::model::{DeclSpace, RefTarget, Span, TargetRoot};
    use crate::track_haskell::extract::extract;
    use std::collections::{BTreeSet, HashSet};

    fn project(roots: &[&str]) -> HsProject {
        HsProject {
            source_roots: roots.iter().map(|r| (*r).to_string()).collect(),
            source_dir_entries: roots.len(),
            packages: ["aeson".to_string()].into_iter().collect(),
            dependencies: ["base".to_string()].into_iter().collect(),
            manifests: vec!["aeson.cabal".to_string()],
            declared_modules: BTreeSet::new(),
        }
    }

    fn import_ref(module_name: &str) -> Reference {
        Reference {
            kind: RefKind::Import,
            space: DeclSpace::Namespace,
            raw_target: module_name.to_string(),
            target: RefTarget {
                root: TargetRoot::Name,
                segments: module_name.split('.').map(str::to_string).collect(),
            },
            locally_bound: false,
            argc: None,
            enclosing: None,
            span: Span {
                byte_start: 0,
                byte_end: 0,
                line: 1,
            },
        }
    }

    fn scope_for(cfg: &HsProject, rel_path: &str) -> HsScope {
        let facts = extract(rel_path, "module M where\n");
        HsResolver.scope(cfg, &facts, &HashSet::<NodeId>::new())
    }

    fn header(rel_path: &str) -> HsHeader {
        HsHeader {
            rel_path: rel_path.to_string(),
            module_name: Some("Data.Aeson".to_string()),
            imports: Vec::new(),
        }
    }

    fn def_of(kind: DefKind, name: &str, owner: &[&str], facets: DefFacets) -> Definition {
        Definition {
            kind,
            name: name.to_string(),
            owner: owner.iter().map(|o| (*o).to_string()).collect(),
            space: DeclSpace::Value,
            facets,
            params: None,
            span: Span {
                byte_start: 0,
                byte_end: 0,
                line: 1,
            },
        }
    }

    #[test]
    fn a_module_is_named_by_its_file_and_a_declaration_by_its_owner_chain() {
        let cfg = project(&["src"]);
        let table: HashSet<NodeId> = HashSet::new();
        let module = def_of(DefKind::Module, "Data.Aeson", &[], DefFacets::SYNTHETIC);
        assert_eq!(
            HsResolver
                .def_fqn(&cfg, &header("src/Data/Aeson.hs"), &[], &module, &table)
                .map(Fqn::into_string),
            Some("src/Data/Aeson".to_string()),
        );
        let ty = def_of(DefKind::Type, "Key", &[], DefFacets::default());
        let ctor = def_of(DefKind::Constructor, "Key", &["Key"], DefFacets::default());
        let field = def_of(DefKind::Field, "unKey", &["Key"], DefFacets::default());
        let h = header("src/Data/Aeson/Key.hs");
        assert_eq!(
            HsResolver
                .def_fqn(&cfg, &h, &[], &ty, &table)
                .map(Fqn::into_string),
            Some("src/Data/Aeson/Key#Key".to_string()),
        );
        // The type and the data constructor share a word and must not share an
        // identity: Haskell's two namespaces, kept apart by nesting.
        assert_eq!(
            HsResolver
                .def_fqn(&cfg, &h, &["Key".to_string()], &ctor, &table)
                .map(Fqn::into_string),
            Some("src/Data/Aeson/Key#Key.Key".to_string()),
        );
        assert_eq!(
            HsResolver
                .def_fqn(&cfg, &h, &["Key".to_string()], &field, &table)
                .map(Fqn::into_string),
            Some("src/Data/Aeson/Key#Key.unKey".to_string()),
        );
    }

    #[test]
    fn six_files_declaring_module_main_are_six_identities() {
        let cfg = project(&["tests", "examples/src"]);
        let table: HashSet<NodeId> = HashSet::new();
        let module = def_of(DefKind::Module, "Main", &[], DefFacets::SYNTHETIC);
        let one = HsResolver.def_fqn(&cfg, &header("tests/Tests.hs"), &[], &module, &table);
        let two = HsResolver.def_fqn(
            &cfg,
            &header("examples/src/Generic.hs"),
            &[],
            &module,
            &table,
        );
        assert_ne!(one, two);
        assert_eq!(one.map(Fqn::into_string), Some("tests/Tests".to_string()));
    }

    #[test]
    fn an_import_resolves_under_the_importing_files_own_root_first() {
        // The measured symlink case: `Data.Aeson.Internal.ByteString` exists
        // under both `src` and `attoparsec-aeson/src`, and each package's
        // import must bind to its own copy.
        let cfg = project(&["src", "attoparsec-aeson/src"]);
        let mut table: HashSet<NodeId> = HashSet::new();
        table.insert(module_id("src/Data/Aeson/Internal/ByteString"));
        table.insert(module_id(
            "attoparsec-aeson/src/Data/Aeson/Internal/ByteString",
        ));

        let from_attoparsec = scope_for(&cfg, "attoparsec-aeson/src/Data/Aeson/Parser/Internal.hs");
        let got = HsResolver.resolve(
            &cfg,
            &from_attoparsec,
            &import_ref("Data.Aeson.Internal.ByteString"),
            &table,
        );
        assert_eq!(
            got.outcome,
            Outcome::Resolved(module_id(
                "attoparsec-aeson/src/Data/Aeson/Internal/ByteString"
            )),
        );

        let from_aeson = scope_for(&cfg, "src/Data/Aeson.hs");
        let got = HsResolver.resolve(
            &cfg,
            &from_aeson,
            &import_ref("Data.Aeson.Internal.ByteString"),
            &table,
        );
        assert_eq!(
            got.outcome,
            Outcome::Resolved(module_id("src/Data/Aeson/Internal/ByteString")),
        );
    }

    #[test]
    fn every_root_probed_is_recorded_hit_and_miss_alike() {
        // The candidate set is what wakes this reference when a later edit
        // declares one of the names it looked for.
        let cfg = project(&["src", "tests"]);
        let mut table: HashSet<NodeId> = HashSet::new();
        table.insert(module_id("tests/Types"));
        let scope = scope_for(&cfg, "tests/Tests.hs");
        let got = HsResolver.resolve(&cfg, &scope, &import_ref("Types"), &table);
        assert_eq!(got.outcome, Outcome::Resolved(module_id("tests/Types")));
        // `tests` first because the importing file sits there, then `src`
        // which was tried and missed — no, `tests` hit first, so only the one
        // probe happened and only it is recorded.
        assert_eq!(got.candidates, [module_id("tests/Types")]);

        let scope = scope_for(&cfg, "src/Data/Aeson.hs");
        let got = HsResolver.resolve(&cfg, &scope, &import_ref("Types"), &table);
        assert_eq!(got.outcome, Outcome::Resolved(module_id("tests/Types")));
        assert_eq!(
            got.candidates,
            [module_id("src/Types"), module_id("tests/Types")],
        );
    }

    #[test]
    fn without_a_source_root_nothing_is_external() {
        // The catastrophic shape this guard exists for: no manifest means no
        // home module, so an unguarded rule would wave the whole denominator
        // out as external and report a rate over nothing.
        let cfg = HsProject::default();
        let table: HashSet<NodeId> = HashSet::new();
        let scope = scope_for(&cfg, "src/M.hs");
        let got = HsResolver.resolve(&cfg, &scope, &import_ref("Data.Text"), &table);
        assert_eq!(
            got.outcome,
            Outcome::Unresolved(UnresolvedReason::ProjectLayoutUnknown),
        );
        assert!(got.candidates.is_empty());
    }

    #[test]
    fn a_module_this_repository_declares_is_never_laundered_as_external() {
        // The batch-1 laundering class, guarded directly: the root map missed
        // the component this module lives in, and the driver knows the name
        // exists because the walk read the file that declares it. Reporting it
        // as `External` would move this build's own bug outside both terms of
        // the rate.
        let mut cfg = project(&["src"]);
        cfg.declared_modules.insert("Twitter.Options".to_string());
        let table: HashSet<NodeId> = HashSet::new();
        let scope = scope_for(&cfg, "src/M.hs");
        let got = HsResolver.resolve(&cfg, &scope, &import_ref("Twitter.Options"), &table);
        assert_eq!(
            got.outcome,
            Outcome::Unresolved(UnresolvedReason::ProjectLayoutUnknown),
        );
        // And the probe it made is still recorded, so declaring the module
        // under that root later wakes this reference.
        assert_eq!(got.candidates, [module_id("src/Twitter/Options")]);
    }

    #[test]
    fn a_repository_that_declares_no_outside_dependency_mints_no_external() {
        let mut cfg = project(&["src"]);
        cfg.dependencies = ["aeson".to_string()].into_iter().collect();
        assert!(!cfg.declares_outside_dependency());
        let table: HashSet<NodeId> = HashSet::new();
        let scope = scope_for(&cfg, "src/M.hs");
        let got = HsResolver.resolve(&cfg, &scope, &import_ref("Data.Text"), &table);
        assert_eq!(
            got.outcome,
            Outcome::Unresolved(UnresolvedReason::UnknownPackage),
        );
    }

    #[test]
    fn an_outsider_is_external_named_by_the_modules_root_segment() {
        let cfg = project(&["src"]);
        let table: HashSet<NodeId> = HashSet::new();
        let scope = scope_for(&cfg, "src/M.hs");
        for (module, want) in [
            ("Data.ByteString", "Data"),
            ("Test.Tasty.HUnit", "Test"),
            ("Prelude", "Prelude"),
        ] {
            let got = HsResolver.resolve(&cfg, &scope, &import_ref(module), &table);
            assert_eq!(got.outcome, Outcome::External(want.to_string()), "{module}");
        }
    }

    #[test]
    fn a_root_matches_whole_segments_only() {
        assert!(under("", "src/M.hs"));
        assert!(under("src", "src/M.hs"));
        assert!(!under("src", "srcfoo/M.hs"));
        assert!(!under("src", "src"));
        assert!(under(
            "text-iso8601/src",
            "text-iso8601/src/Data/Time/ToText.hs"
        ));
    }

    #[test]
    fn a_signature_and_its_equation_are_one_declaration() {
        let sig = def_of(DefKind::Function, "toJSON", &[], DefFacets::default());
        let eqn = def_of(DefKind::Function, "toJSON", &[], DefFacets::default());
        let other = def_of(DefKind::Method, "toJSON", &["ToJSON"], DefFacets::default());
        assert!(HsResolver.mergeable(&sig, &eqn));
        assert!(!HsResolver.mergeable(&sig, &other));
    }

    #[test]
    fn a_module_node_is_a_package_and_a_declaration_is_not() {
        let module = def_of(DefKind::Module, "Data.Aeson", &[], DefFacets::SYNTHETIC);
        let ty = def_of(DefKind::Type, "Value", &[], DefFacets::default());
        assert!(HsResolver.stores_as_package(&module));
        assert!(!HsResolver.stores_as_package(&ty));
    }
}
