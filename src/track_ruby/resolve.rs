//! The one place a Ruby [`crate::Outcome`] is produced. Never drops.
//!
//! # The import model, as measured
//!
//! Three rules, and each of them is a rule about *where a name is looked up*
//! rather than about what it might mean:
//!
//! 1. **`require_relative <literal>`** resolves against the directory of the
//!    requiring file. It is the only form that can reach a file outside every
//!    load root, which is how a test tree reaches its own `helper.rb`.
//! 2. **`require <literal>`** and **`autoload :C, <literal>`** resolve against
//!    each load root in order — the gemspec's `require_paths`, `lib` by
//!    default. `autoload` is the same lookup deferred to first constant
//!    reference, and the deferral is a runtime fact, not a resolution one.
//! 3. **A specifier that is not a literal** resolves against nothing.
//!    [`UnresolvedReason::DynamicModuleSpecifier`], never a guess: a
//!    `require` built by interpolation names a file only the running program
//!    knows.
//!
//! # Why a miss outside the repository is `UnknownPackage` and not `External`
//!
//! `require 'time'` names Ruby's standard library, and `require 'simplecov'`
//! names a gem. Both are outside this repository and both are real; neither
//! is *indexed*, and this build holds no frozen standard-library name set to
//! tell them apart from a load root this resolver got wrong. So only a name
//! the project's own gemspec declares as a dependency is `External`, and
//! everything else is [`UnresolvedReason::UnknownPackage`] and counts against
//! the rate.
//!
//! That is the deliberately expensive answer. `External` sits outside *both*
//! terms of the resolution rate, so widening it is the cheapest way there is
//! to raise a rate without linking anything — and a standard-library set
//! written from memory rather than measured is exactly how that widening
//! would arrive. The reason's own definition is the honest fit: the target
//! names a package outside the repository **that was not indexed**.
//!
//! # `LocalBinding` does not apply here
//!
//! Tier 2 emits no expression-level reference, so no Ruby reference can name
//! a parameter, a local or a receiver. The bucket stays empty, and the
//! baseline records it as zero — which makes this track's rate un-gameable by
//! the one reclassification the rate's own definition permits.

use std::collections::HashMap;
use std::path::Path;

use crate::UnresolvedReason;
use crate::lang::{FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe};
use crate::model::{
    DefFacets, DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, Reference, node_id,
};
use crate::track_ruby::extract::{ImportForm, RubyExtractor, RubyHeader};
use crate::track_ruby::lang::{RubyLang, feature_fqn};
use crate::track_ruby::project::{RubyProject, layout};

/// One file's view of what its own imports mean.
///
/// Two facts and no more: where the file sits, which is what a
/// `require_relative` is relative to, and what each of its import clauses
/// spells, keyed by the span the clause shares with its reference.
pub struct RubyScope {
    /// The file's directory, repo-relative, without a trailing slash.
    dir: String,
    /// Each import clause's form, by `(byte_start, byte_end)` of its call.
    imports: HashMap<(u32, u32), ImportForm>,
}

/// An outcome with nothing probed.
fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: crate::Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// A specifier with the optional `.rb` Ruby lets it carry removed.
///
/// `require 'utils.rb'` and `require 'utils'` name one feature. Only `.rb` is
/// stripped: `.so` and `.bundle` name a compiled extension, which is not a
/// file this walk reads and not one it may pretend to have found.
fn without_rb(spec: &str) -> &str {
    spec.strip_suffix(".rb").unwrap_or(spec)
}

/// Join a repo-relative directory and a specifier, resolving `.` and `..`.
///
/// `None` when the result would escape above the repository root — a real
/// `require_relative '../../elsewhere'` reaching out of the tree — because a
/// path this scan cannot see is not one it may claim to have resolved.
fn join_path(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    for segment in spec.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// The identity of the feature a repo-relative path (without `.rb`) is.
fn feature_id(path: &str) -> NodeId {
    node_id(Domain::Ruby, &feature_fqn(path))
}

/// Ruby's resolver. Stateless: everything it reads is in the config, the
/// scope, or the probe.
pub struct RubyResolver;

impl RubyResolver {
    /// `require_relative`: one candidate, against the requiring file.
    fn relative(scope: &RubyScope, spec: &str, probe: &dyn SymbolProbe) -> Resolution {
        let Some(path) = join_path(&scope.dir, without_rb(spec)) else {
            return unresolved(UnresolvedReason::ModuleNotFound);
        };
        let id = feature_id(&path);
        let outcome = if probe.probe(&id).is_some() {
            crate::Outcome::Resolved(id)
        } else {
            // The lookup was complete — every `.rb` file in the tree is a
            // feature node — and the literal named none of them.
            crate::Outcome::Unresolved(UnresolvedReason::ModuleNotFound)
        };
        Resolution {
            outcome,
            candidates: vec![id],
        }
    }

    /// `require` and `autoload`: each load root in order, then the question of
    /// what a total miss means.
    fn load_path(cfg: &RubyProject, spec: &str, probe: &dyn SymbolProbe) -> Resolution {
        let spec = without_rb(spec);
        let mut roots: Vec<&str> = Vec::new();
        if spec.starts_with('/') {
            // An absolute filesystem path is outside this repository by
            // construction; nothing in the tree can be probed for it.
        } else if spec.starts_with("./") || spec.starts_with("../") {
            // Anchored at the working directory rather than at the load path.
            // The repository root is what a script run from the repository
            // sees, and it is the only anchor this scan can name.
            roots.push("");
        } else {
            roots.extend(cfg.load_roots.iter().map(String::as_str));
        }

        let mut candidates = Vec::new();
        for root in roots {
            let Some(path) = join_path(root, spec) else {
                continue;
            };
            let id = feature_id(&path);
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
        let outcome = match cfg.declared_gem(spec) {
            Some(gem) => crate::Outcome::External(gem.to_string()),
            None => crate::Outcome::Unresolved(UnresolvedReason::UnknownPackage),
        };
        Resolution {
            outcome,
            candidates,
        }
    }
}

impl Resolver<RubyLang> for RubyResolver {
    fn config(&self, root: &Path, _files: &FileIndex) -> Result<RubyProject, LayoutError> {
        layout(root)
    }

    fn config_digest(&self, cfg: &RubyProject) -> Vec<u8> {
        // The load roots root every candidate a `require` builds, so a scan
        // under a different set describes a different graph and cannot be
        // patched into this one file by file.
        cfg.digest()
    }

    fn declared_container(
        &self,
        _cfg: &RubyProject,
        _header: &RubyHeader,
    ) -> Option<(String, String)> {
        // A Ruby file names no container for anybody else: `module Rack`
        // reopens a constant every other file may reopen too, and the feature
        // a file *is* comes from its path rather than from its source.
        None
    }

    fn learn_containers(&self, _cfg: &mut RubyProject, _names: &HashMap<String, String>) {
        // Nothing a Ruby reference binds is derived from another file's
        // source, so there is nothing to learn.
    }

    fn owns_file(&self, _cfg: &RubyProject, _rel_path: &str) -> bool {
        // No nested-manifest fence: a gemspec in a subdirectory is a shape
        // phase 0 does not read, so no file is excluded on account of one.
        true
    }

    fn def_fqn(
        &self,
        _cfg: &RubyProject,
        header: &RubyHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        // The file's own feature node: synthesized, at the top level, and a
        // module. Its identity is the path, because that is what a `require`
        // spells and what `$LOADED_FEATURES` holds.
        if def.kind == DefKind::Module
            && def.facets.contains(DefFacets::SYNTHETIC)
            && owner.is_empty()
        {
            return Some(Fqn::new(feature_fqn(&header.rel_path)));
        }
        // An encloser arrives as a synthetic definition carrying only a path,
        // so a singleton method's name is spelled `self.m` there and its
        // facets are empty. Reading both is what keeps the identity an edge
        // starts at equal to the one the definition was filed under.
        let (name, singleton) = match def.name.strip_prefix("self.") {
            Some(rest) => (rest, true),
            None => (def.name.as_str(), def.facets.contains(DefFacets::STATIC)),
        };
        if name.is_empty() {
            return None;
        }
        let scope = owner.join("::");
        Some(Fqn::new(match def.kind {
            DefKind::Module | DefKind::Type | DefKind::Const => {
                if scope.is_empty() {
                    name.to_string()
                } else {
                    format!("{scope}::{name}")
                }
            }
            // A top-level `def` is a private instance method of `Object`,
            // which is where a name for it has to come from: the file is not
            // its owner, and inventing one per file would give one method as
            // many identities as files declare it.
            _ => {
                let owner = if scope.is_empty() { "Object" } else { &scope };
                let sep = if singleton { '.' } else { '#' };
                format!("{owner}{sep}{name}")
            }
        }))
    }

    fn index_keys(&self, _cfg: &RubyProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // Every Ruby node is reachable by exactly one identity: a feature by
        // its path, a constant by its scoped name.
        Vec::new()
    }

    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        // Reopening is ordinary Ruby, not corruption: `module Rack` is
        // written once per file across a whole gem and there is one `Rack` at
        // runtime. Two declarations of the same kind, name and owner are that
        // one entity; a class and a module under one name are not, and that
        // really is the collision this count exists to surface.
        a.kind == b.kind && a.name == b.name && a.owner == b.owner
    }

    fn scope(
        &self,
        _cfg: &RubyProject,
        file: &FileFacts<RubyLang>,
        _probe: &dyn SymbolProbe,
    ) -> RubyScope {
        let rel = &file.header.rel_path;
        let dir = match rel.rfind('/') {
            Some(at) => rel[..at].to_string(),
            None => String::new(),
        };
        RubyScope {
            dir,
            imports: file
                .header
                .imports
                .iter()
                .map(|i| ((i.span.byte_start, i.span.byte_end), i.form.clone()))
                .collect(),
        }
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // Tier 2 emits no `Inherit` reference: `class C < Base` is part of
        // `C`'s structure here and is not resolved, so there is no supertype
        // relation to build and nothing for the driver to run a phase over.
        &[]
    }

    fn resolve(
        &self,
        cfg: &RubyProject,
        scope: &RubyScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        match scope.imports.get(&(r.span.byte_start, r.span.byte_end)) {
            Some(ImportForm::Relative(spec)) => Self::relative(scope, spec, probe),
            Some(ImportForm::LoadPath(spec)) => Self::load_path(cfg, spec, probe),
            // A clause whose specifier could not be read as one literal, and
            // — unreachable, since the extractor emits a clause and its
            // reference together — a reference with no clause at all. Both
            // mean the same thing: this build cannot say which file is named,
            // and it will not guess one.
            Some(ImportForm::Dynamic) | None => {
                unresolved(UnresolvedReason::DynamicModuleSpecifier)
            }
        }
    }
}

/// The Ruby track's scan entry point, reading every `.rb` the walk finds.
pub fn scan_ruby(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_ruby_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_ruby`] under a repository's include/exclude globs. What
/// [`crate::track_ruby::TRACK`] holds.
pub fn scan_ruby_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<RubyLang>(root, db, &RubyExtractor, &RubyResolver, filter)
}

/// Ruby's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(RubyLang::LANG, Lang::Ruby));
    assert!(matches!(RubyLang::DOMAIN, Domain::Ruby));
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeclSpace, Span};
    use crate::track_ruby::extract::extract;
    use std::collections::HashSet;

    fn project(roots: &[&str]) -> RubyProject {
        RubyProject {
            load_roots: roots.iter().map(|r| (*r).to_string()).collect(),
            dependencies: Default::default(),
            gemspecs: Vec::new(),
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

    fn header() -> RubyHeader {
        RubyHeader {
            rel_path: "lib/rack/request.rb".to_string(),
            imports: Vec::new(),
        }
    }

    #[test]
    fn a_path_that_climbs_past_the_root_has_no_answer() {
        assert_eq!(join_path("lib", "../a"), Some("a".to_string()));
        assert_eq!(join_path("lib", "../../a"), None);
        assert_eq!(join_path("", "a/b"), Some("a/b".to_string()));
        assert_eq!(join_path("lib/rack", "./x"), Some("lib/rack/x".to_string()));
    }

    #[test]
    fn only_the_rb_suffix_is_optional() {
        assert_eq!(without_rb("utils.rb"), "utils");
        assert_eq!(without_rb("utils"), "utils");
        assert_eq!(without_rb("ext.so"), "ext.so");
    }

    #[test]
    fn the_feature_node_is_named_by_the_path_and_a_constant_by_its_scope() {
        let cfg = project(&["lib"]);
        let table: HashSet<NodeId> = HashSet::new();
        let feature = def_of(DefKind::Module, "request", DefFacets::SYNTHETIC);
        assert_eq!(
            RubyResolver
                .def_fqn(&cfg, &header(), &[], &feature, &table)
                .map(Fqn::into_string),
            Some("$lib/rack/request".to_string()),
        );
        let module = def_of(DefKind::Module, "Rack", DefFacets::default());
        assert_eq!(
            RubyResolver
                .def_fqn(&cfg, &header(), &[], &module, &table)
                .map(Fqn::into_string),
            Some("Rack".to_string()),
        );
    }

    #[test]
    fn a_singleton_method_and_an_instance_method_of_one_name_are_two_nodes() {
        let cfg = project(&["lib"]);
        let table: HashSet<NodeId> = HashSet::new();
        let owner = vec!["Rack".to_string(), "Request".to_string()];
        let instance = def_of(DefKind::Method, "parse", DefFacets::default());
        let singleton = def_of(DefKind::Method, "parse", DefFacets::STATIC);
        let a = RubyResolver
            .def_fqn(&cfg, &header(), &owner, &instance, &table)
            .map(Fqn::into_string);
        let b = RubyResolver
            .def_fqn(&cfg, &header(), &owner, &singleton, &table)
            .map(Fqn::into_string);
        assert_eq!(a, Some("Rack::Request#parse".to_string()));
        assert_eq!(b, Some("Rack::Request.parse".to_string()));
    }

    #[test]
    fn an_enclosers_self_prefixed_name_spells_the_same_identity() {
        // What `Encloser::as_definition` hands back for `def self.parse_file`:
        // a plain definition whose name carries the marker and whose facets
        // are empty. It must name the node the definition phase filed.
        let cfg = project(&["lib"]);
        let table: HashSet<NodeId> = HashSet::new();
        let owner = vec!["Rack".to_string(), "Builder".to_string()];
        let from_encloser = def_of(DefKind::Method, "self.parse_file", DefFacets::default());
        let from_definition = def_of(DefKind::Method, "parse_file", DefFacets::STATIC);
        assert_eq!(
            RubyResolver.def_fqn(&cfg, &header(), &owner, &from_encloser, &table),
            RubyResolver.def_fqn(&cfg, &header(), &owner, &from_definition, &table),
        );
    }

    #[test]
    fn a_reopened_module_is_one_entity_and_a_class_of_the_same_name_is_not() {
        let a = def_of(DefKind::Module, "Rack", DefFacets::default());
        let b = def_of(DefKind::Module, "Rack", DefFacets::default());
        let c = def_of(DefKind::Type, "Rack", DefFacets::default());
        assert!(RubyResolver.mergeable(&a, &b));
        assert!(!RubyResolver.mergeable(&a, &c));
    }

    #[test]
    fn every_import_reference_is_paired_with_a_clause() {
        // The pairing is by span, so a reference the scope cannot find would
        // silently become `DynamicModuleSpecifier` for a perfectly literal
        // specifier. It must be total.
        let cfg = project(&["lib"]);
        let table: HashSet<NodeId> = HashSet::new();
        let source =
            "require 'a'\nrequire_relative 'b'\nrequire path\nmodule M\n  autoload :C, 'c'\nend\n";
        let facts = extract("lib/x.rb", source);
        let scope = RubyResolver.scope(&cfg, &facts, &table);
        assert_eq!(facts.refs.len(), 4);
        for r in &facts.refs {
            assert!(
                scope
                    .imports
                    .contains_key(&(r.span.byte_start, r.span.byte_end)),
                "unpaired: {}",
                r.raw_target,
            );
        }
    }
}
