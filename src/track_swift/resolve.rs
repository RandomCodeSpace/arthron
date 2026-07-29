//! The one place a Swift [`crate::Outcome`] is produced. Never drops.
//!
//! # The import model, as measured
//!
//! A Swift `import` names a **module**, and a module is a whole SwiftPM
//! target. The package's manifest enumerates every target it builds, so the
//! module namespace is not inferred — it is *stated*, and it is closed. Three
//! rules follow, and each is a rule about where a name can possibly live:
//!
//! 1. **The enumeration this build read is not whole.** Either the manifest
//!    stated no target this reader could read, or it stated one it could not
//!    — a `.target(name: computed)`, or a `Package.swift` of another package
//!    nested in the tree. Then there is a module arthron cannot name, and
//!    neither "inside this repository" nor "outside it" is a thing it may
//!    assert about a name it does not recognise.
//!    [`UnresolvedReason::ProjectLayoutUnknown`], which says exactly that: the
//!    failure is arthron's own inference rather than a name that is absent.
//! 2. **The first segment names a target.** The module is in this repository,
//!    so the identity is probed. A plain `import Alamofire` probes the module
//!    node; `import struct Alamofire.Session` probes the declaration inside
//!    it. A miss is [`UnresolvedReason::NoMatchingDefinition`] — the lookup
//!    really was complete, because every walked file of that target declares
//!    the module — and in a package that compiles it means arthron's bug.
//! 3. **The first segment names anything else, and rule 1 did not fire.** The
//!    module is outside this package: [`crate::Outcome::External`].
//!
//! # Why "outside this package" is `External` here and `UnknownPackage` in Ruby
//!
//! The two tracks answer the same question differently, and the difference is
//! not taste. A Ruby `require 'time'` names a **file on a load path arthron
//! infers**, so a miss is ambiguous between "outside the repository" and "our
//! load-root inference is wrong" — and Ruby therefore refuses to say
//! "outside", because `External` sits outside *both* terms of the resolution
//! rate and widening it is the cheapest way there is to raise a rate with
//! nothing linked.
//!
//! Swift has no such ambiguity. `import Foundation` names a module, not a
//! path; there is no lookup for arthron to get wrong, and the set of modules
//! this package builds is written down in the manifest rather than deduced
//! from the tree. A name that is not one of them is outside the package by
//! construction.
//!
//! That claim is only as good as having read the enumeration **whole**, which
//! is why rule 1 is not a formality and why it is
//! [`SwiftPackage::complete`] rather than `known` that it asks. With no target
//! read, nothing is classified `External`; with four targets read out of five,
//! nothing is either, because the fifth module is in this repository and would
//! otherwise be spelled exactly the way `Foundation` is. The all-or-nothing
//! version of this guard covered only the first case, and the partial read is
//! the one a reader of a manifest is actually likely to hit.
//!
//! The corpus test pins the external set by name and the gate fails on drift
//! in the `External` count, so a target this reader ever stops seeing shows up
//! as a new external name and a moved bucket rather than as a reference that
//! quietly left the measurement.
//!
//! # What has no reference at all
//!
//! Not one of the 43 files in the measured corpus's `Source/` imports the
//! module it belongs to; all 43 *are* it, and each sees the other 42's
//! top-level names with no import, no path and no qualifier anywhere in the
//! referencing file. There is no site, so there is no reference — arthron
//! emits nothing rather than synthesising 43×42 of them, and the honest
//! consequence is that Swift's denominator is small. See
//! [`crate::track_swift`] for what that means for reading the rate.
//!
//! # `LocalBinding` does not apply here
//!
//! Tier 2 emits no expression-level reference, so no Swift reference can name
//! a parameter, a local or a receiver. The bucket stays empty, and the
//! baseline records it as zero — which makes this track's rate un-gameable by
//! the one reclassification the rate's own definition permits.

use std::collections::HashMap;
use std::path::Path;

use crate::UnresolvedReason;
use crate::lang::{FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe};
use crate::model::{
    DefFacets, DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, Reference, TargetRoot,
    node_id,
};
use crate::track_swift::extract::{SwiftExtractor, SwiftHeader};
use crate::track_swift::lang::SwiftLang;
use crate::track_swift::project::{SwiftPackage, layout};

/// Swift's per-file scope: nothing.
///
/// An `import` names a module by name, and the module namespace belongs to the
/// package rather than to the file — so which file an import sits in changes
/// nothing about what it can name. Ruby needs the requiring file's directory
/// and Rust needs the file's crate; Swift needs neither, and an empty scope
/// says so rather than carrying a fact no rule reads.
pub struct SwiftScope;

/// An outcome with nothing probed.
fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: crate::Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// Swift's resolver. Stateless: everything it reads is in the config or the
/// probe.
pub struct SwiftResolver;

impl Resolver<SwiftLang> for SwiftResolver {
    fn config(&self, root: &Path, files: &FileIndex) -> Result<SwiftPackage, LayoutError> {
        layout(root, files)
    }

    fn config_digest(&self, cfg: &SwiftPackage) -> Vec<u8> {
        // The target list decides every module identity in the graph, so a
        // scan under a different one describes a different package and cannot
        // be patched into this one file by file.
        cfg.digest()
    }

    fn declared_container(
        &self,
        _cfg: &SwiftPackage,
        _header: &SwiftHeader,
    ) -> Option<(String, String)> {
        // A Swift file names no container, for itself or for anybody else:
        // module membership is decided by the manifest's target definition and
        // by nothing written in the source. That is the measured fact this
        // whole track is shaped around.
        None
    }

    fn learn_containers(&self, _cfg: &mut SwiftPackage, _names: &HashMap<String, String>) {
        // Nothing a Swift reference binds is derived from another file's
        // source, so there is nothing to learn.
    }

    fn owns_file(&self, _cfg: &SwiftPackage, _rel_path: &str) -> bool {
        // No nested-manifest fence. A file no target claims is still a Swift
        // file the walk read, and dropping it would lose the declarations it
        // makes — the package manifests themselves are exactly that shape.
        true
    }

    fn def_fqn(
        &self,
        cfg: &SwiftPackage,
        header: &SwiftHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        let module = cfg.module_fqn(&header.rel_path);
        // The file's module placeholder: synthesized, at the top level, and
        // carrying no name, because no Swift file states which module it is
        // in. Its identity comes from the manifest and the path together.
        if def.kind == DefKind::Module
            && def.facets.contains(DefFacets::SYNTHETIC)
            && owner.is_empty()
        {
            return Some(Fqn::new(module));
        }
        if def.name.is_empty() {
            return None;
        }
        let mut parts = Vec::with_capacity(owner.len() + 2);
        parts.push(module);
        parts.extend(owner.iter().cloned());
        parts.push(def.name.clone());
        Some(Fqn::new(parts.join(".")))
    }

    fn index_keys(&self, _cfg: &SwiftPackage, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // Every Swift node is reachable by exactly one identity: a module by
        // its target name, everything else by its dotted path below it.
        Vec::new()
    }

    fn mergeable(&self, _a: &Definition, _b: &Definition) -> bool {
        // Two declarations sharing an FQN are two entities, never one. The
        // corpus has 74 `#if` blocks and both arms are read as written, so
        // `#if canImport(Security)` / `#else` twins of one member are two real
        // declarations — merging them would hide that the graph holds a union
        // over configurations, which is the one thing a reader of it has to
        // know.
        false
    }

    fn scope(
        &self,
        _cfg: &SwiftPackage,
        _file: &FileFacts<SwiftLang>,
        _probe: &dyn SymbolProbe,
    ) -> SwiftScope {
        SwiftScope
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // Tier 2 emits no `Inherit` reference: `class C: Base` is part of
        // `C`'s structure here and is not resolved, so there is no supertype
        // relation to build and nothing for the driver to run a phase over.
        &[]
    }

    fn resolve(
        &self,
        cfg: &SwiftPackage,
        _scope: &SwiftScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        // An import path is a dotted list of identifiers or it is not an
        // import; the extractor emits nothing else, and a reference that
        // arrived any other way names no module this build can read.
        let segments = match r.target.root {
            TargetRoot::Name => &r.target.segments,
            _ => return unresolved(UnresolvedReason::DynamicModuleSpecifier),
        };
        let Some(module) = segments.first() else {
            return unresolved(UnresolvedReason::DynamicModuleSpecifier);
        };
        if !cfg.is_target(module) {
            // Rule 1, and it guards rule 3 rather than standing beside it:
            // "outside this package" is a claim about a *whole* enumeration,
            // so a build that read none of the targets and a build that read
            // all but one both have to decline to make it. Declining costs a
            // reference in the rate's denominator; making it wrongly moves an
            // in-repository module into `External`, outside both of its terms,
            // where nothing can see it go.
            if !cfg.complete() {
                return unresolved(UnresolvedReason::ProjectLayoutUnknown);
            }
            return Resolution {
                outcome: crate::Outcome::External(module.clone()),
                candidates: Vec::new(),
            };
        }
        let id = node_id(Domain::Swift, &segments.join("."));
        let outcome = if probe.probe(&id).is_some() {
            crate::Outcome::Resolved(id)
        } else {
            // The module is one this package builds and the lookup was
            // complete — every walked file of a target declares its module —
            // so the name really is absent from the graph.
            crate::Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
        };
        Resolution {
            outcome,
            candidates: vec![id],
        }
    }
}

/// The Swift track's scan entry point, reading every `.swift` the walk finds.
pub fn scan_swift(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_swift_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_swift`] under a repository's include/exclude globs. What
/// [`crate::track_swift::TRACK`] holds.
pub fn scan_swift_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<SwiftLang>(root, db, &SwiftExtractor, &SwiftResolver, filter)
}

/// Swift's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(SwiftLang::LANG, Lang::Swift));
    assert!(matches!(SwiftLang::DOMAIN, Domain::Swift));
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeclSpace, Span};
    use crate::track_swift::project::{Target, TargetKind};
    use std::collections::HashSet;

    fn package(targets: &[(&str, TargetKind, &str)]) -> SwiftPackage {
        SwiftPackage {
            name: "Alamofire".to_string(),
            manifests: vec!["Package.swift".to_string()],
            manifest: "Package.swift".to_string(),
            tools_version: "6.3".to_string(),
            unread: Vec::new(),
            targets: targets
                .iter()
                .map(|(name, kind, dir)| Target {
                    name: (*name).to_string(),
                    kind: *kind,
                    dir: (*dir).to_string(),
                    excludes: Vec::new(),
                    sources: Vec::new(),
                })
                .collect(),
        }
    }

    fn header(rel: &str) -> SwiftHeader {
        SwiftHeader {
            rel_path: rel.to_string(),
            imports: Vec::new(),
            extensions: Vec::new(),
        }
    }

    fn def_of(kind: DefKind, name: &str, facets: DefFacets) -> Definition {
        Definition {
            kind,
            name: name.to_string(),
            owner: Vec::new(),
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
    fn a_module_is_named_by_its_target_and_a_member_by_its_path_below_it() {
        let cfg = package(&[("Alamofire", TargetKind::Regular, "Source")]);
        let table: HashSet<NodeId> = HashSet::new();
        let module = def_of(DefKind::Module, "", DefFacets::SYNTHETIC);
        assert_eq!(
            SwiftResolver
                .def_fqn(
                    &cfg,
                    &header("Source/Core/Session.swift"),
                    &[],
                    &module,
                    &table
                )
                .map(Fqn::into_string),
            Some("Alamofire".to_string()),
        );
        let method = def_of(DefKind::Method, "request(_:method:)", DefFacets::default());
        assert_eq!(
            SwiftResolver
                .def_fqn(
                    &cfg,
                    &header("Source/Core/Session.swift"),
                    &["Session".to_string()],
                    &method,
                    &table,
                )
                .map(Fqn::into_string),
            Some("Alamofire.Session.request(_:method:)".to_string()),
        );
    }

    #[test]
    fn a_member_an_extension_declares_on_a_foreign_type_is_named_under_this_module() {
        // `extension URLRequest { … }` in Alamofire declares a member of
        // Foundation's type. The member is the repository's; the type is not,
        // and the identity says both.
        let cfg = package(&[("Alamofire", TargetKind::Regular, "Source")]);
        let table: HashSet<NodeId> = HashSet::new();
        let method = def_of(DefKind::Method, "af", DefFacets::default());
        assert_eq!(
            SwiftResolver
                .def_fqn(
                    &cfg,
                    &header("Source/Extensions/URLRequest+Alamofire.swift"),
                    &["URLRequest".to_string()],
                    &method,
                    &table,
                )
                .map(Fqn::into_string),
            Some("Alamofire.URLRequest.af".to_string()),
        );
    }

    #[test]
    fn a_file_no_target_claims_is_its_own_module_and_cannot_collide_with_one() {
        let cfg = package(&[("Alamofire", TargetKind::Regular, "Source")]);
        let table: HashSet<NodeId> = HashSet::new();
        let module = def_of(DefKind::Module, "", DefFacets::SYNTHETIC);
        let a = SwiftResolver
            .def_fqn(&cfg, &header("Package.swift"), &[], &module, &table)
            .map(Fqn::into_string);
        let b = SwiftResolver
            .def_fqn(
                &cfg,
                &header("Package@swift-6.0.swift"),
                &[],
                &module,
                &table,
            )
            .map(Fqn::into_string);
        assert_eq!(a, Some("$Package".to_string()));
        assert_eq!(b, Some("$Package@swift-6.0".to_string()));
        // Four manifests declaring `let package` are four declarations, not
        // one: SwiftPM compiles each as its own module.
        assert_ne!(a, b);
    }

    fn import_ref(segments: &[&str]) -> Reference {
        Reference {
            kind: RefKind::Import,
            space: DeclSpace::Namespace,
            raw_target: format!("import {}", segments.join(".")),
            target: crate::model::RefTarget {
                root: TargetRoot::Name,
                segments: segments.iter().map(|s| (*s).to_string()).collect(),
            },
            locally_bound: false,
            argc: None,
            arg_types: None,
            enclosing: None,
            span: Span {
                byte_start: 0,
                byte_end: 0,
                line: 1,
            },
        }
    }

    #[test]
    fn an_import_of_a_target_resolves_and_an_import_of_anything_else_is_external() {
        let cfg = package(&[
            ("Alamofire", TargetKind::Regular, "Source"),
            ("AlamofireTests", TargetKind::Test, "Tests"),
        ]);
        let mut table: HashSet<NodeId> = HashSet::new();
        let id = node_id(Domain::Swift, "Alamofire");
        table.insert(id);
        assert_eq!(
            SwiftResolver
                .resolve(&cfg, &SwiftScope, &import_ref(&["Alamofire"]), &table)
                .outcome,
            crate::Outcome::Resolved(id),
        );
        assert_eq!(
            SwiftResolver
                .resolve(&cfg, &SwiftScope, &import_ref(&["Foundation"]), &table)
                .outcome,
            crate::Outcome::External("Foundation".to_string()),
        );
    }

    #[test]
    fn without_a_target_list_nothing_is_laundered_into_external() {
        // The guard. `External` sits outside both terms of the rate, so a
        // manifest this build could not read must not be able to move every
        // import in the package into it.
        let cfg = SwiftPackage::default();
        let table: HashSet<NodeId> = HashSet::new();
        assert_eq!(
            SwiftResolver
                .resolve(&cfg, &SwiftScope, &import_ref(&["Foundation"]), &table)
                .outcome,
            crate::Outcome::Unresolved(UnresolvedReason::ProjectLayoutUnknown),
        );
    }

    #[test]
    fn an_import_of_a_target_whose_node_is_missing_says_so_rather_than_going_external() {
        // A target the manifest declares but this scan indexed no file of.
        // The lookup was complete, so the honest answer blames arthron.
        let cfg = package(&[("Empty", TargetKind::Regular, "Sources/Empty")]);
        let table: HashSet<NodeId> = HashSet::new();
        assert_eq!(
            SwiftResolver
                .resolve(&cfg, &SwiftScope, &import_ref(&["Empty"]), &table)
                .outcome,
            crate::Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
        );
    }

    #[test]
    fn a_declaration_import_probes_the_declaration_and_keeps_the_module_outside() {
        let cfg = package(&[("Alamofire", TargetKind::Regular, "Source")]);
        let mut table: HashSet<NodeId> = HashSet::new();
        let session = node_id(Domain::Swift, "Alamofire.Session");
        table.insert(session);
        // `import struct Alamofire.Session` names the declaration, not the
        // module, and the identity probed is the declaration's.
        assert_eq!(
            SwiftResolver
                .resolve(
                    &cfg,
                    &SwiftScope,
                    &import_ref(&["Alamofire", "Session"]),
                    &table,
                )
                .outcome,
            crate::Outcome::Resolved(session),
        );
        // `import struct Foundation.Data` names a declaration in a module
        // outside the package; the package is what the reference is external
        // to.
        assert_eq!(
            SwiftResolver
                .resolve(
                    &cfg,
                    &SwiftScope,
                    &import_ref(&["Foundation", "Data"]),
                    &table,
                )
                .outcome,
            crate::Outcome::External("Foundation".to_string()),
        );
    }
}
