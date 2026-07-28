//! The one place a Dart [`crate::Outcome`] is produced. Never drops.
//!
//! # The import model, as measured
//!
//! Every Dart directive names a library by URI, and the URI's *scheme* is what
//! decides where the lookup happens. Four rules, and each is a rule about
//! where a name is looked up rather than about what it might mean:
//!
//! 1. **`dart:<library>`** names an SDK library. Always [`crate::Outcome::External`].
//! 2. **`package:<name>/<path>`** names a file in package `<name>`'s `lib/`
//!    directory, and the manifest is the only thing that says where that
//!    directory is. When `<name>` is what `pubspec.yaml` calls *this* package
//!    the lookup is in-repository — `lib/<path>` — and a miss is
//!    [`UnresolvedReason::ModuleNotFound`]. When the manifest declares
//!    `<name>` with a `path:` it has placed the package inside this
//!    repository too, so the lookup is `<dir>/lib/<path>` and a miss is
//!    `ModuleNotFound` again. When it declares `<name>` any other way — pub,
//!    git, hosted — it is `External`. When it does not declare it at all it
//!    is [`UnresolvedReason::UnknownPackage`], and when there is no manifest
//!    at all it is [`UnresolvedReason::ProjectLayoutUnknown`] — arthron's own
//!    gap, not a statement about the name.
//! 3. **A relative URI** resolves against the directory of the referring file,
//!    which is how a library under `lib/src/` reaches its sibling and how a
//!    test reaches `../unmodifiable_collection_test.dart`.
//! 4. **A URI that is not one literal** resolves against nothing:
//!    [`UnresolvedReason::DynamicModuleSpecifier`], never a guess.
//!
//! # Why `dart:` is `External` and Ruby's `require 'time'` is not
//!
//! The two look like the same question — "the standard library is outside the
//! repository, what now?" — and they get opposite answers, for a reason worth
//! stating because `External` sits outside *both* terms of the resolution rate
//! and is therefore the cheapest way there is to raise a rate with nothing
//! linked.
//!
//! Ruby's `require 'time'` and a `require` whose load root this resolver got
//! wrong are spelled identically; telling them apart would need a frozen
//! standard-library name set nothing here has measured, so the honest answer
//! is `UnknownPackage`, counted *against* the rate.
//!
//! Dart states it in the grammar. `dart:` is a reserved scheme: no file in any
//! repository can be addressed by one, and no `pubspec.yaml` can claim it. So
//! classifying a `dart:` URI as external cannot launder an in-repository file
//! into the bucket outside the rate — the thing it names provably is not one.
//! The named package is the whole URI, `dart:collection` and not `collection`,
//! because a package called `collection` is exactly what this corpus's own
//! `pubspec.yaml` declares and the two must never share a node.
//!
//! The laundering that *is* possible here is the `package:` one, and rule 2 is
//! written in the order that prevents it. This repository's own package name
//! is tested **first**, so `package:collection/src/algorithms.dart` is a
//! lookup in `lib/` that can miss, and never an `External` that cannot. A
//! dependency the manifest places inside the tree with a `path:` is tested the
//! same way and for the same reason: the files behind such a URI are ones the
//! walk reached and stored, so answering it `External` would move a reference
//! whose target *is* an in-repository node out of both terms of the rate.
//! Getting that second one wrong is not cosmetic. In a multi-package
//! repository — melos, a federated plugin — the cross-package imports are
//! exactly the linking that matters, and calling every one of them external
//! leaves a track that links nothing between packages printing a full rate.
//!
//! # `LocalBinding` does not apply here
//!
//! Tier 2 emits no expression-level reference, so no Dart reference can name a
//! parameter, a local or a receiver. The bucket stays empty, and the baseline
//! records it as zero — which makes this track's rate un-gameable by the one
//! reclassification the rate's own definition permits.

use std::collections::HashMap;
use std::path::Path;

use crate::UnresolvedReason;
use crate::lang::{FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe};
use crate::model::{
    DefFacets, DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, Reference, node_id,
};
use crate::track_dart::extract::{DartExtractor, DartHeader, UriForm};
use crate::track_dart::lang::{DartLang, decl_fqn, library_fqn};
use crate::track_dart::project::{DartDep, DartProject, LIB, layout};

/// The scheme an SDK library URI carries.
const SDK_SCHEME: &str = "dart";

/// The scheme a package URI carries.
const PACKAGE_SCHEME: &str = "package";

/// One file's view of what its own directives mean.
///
/// Two facts and no more: where the file sits, which is what a relative URI is
/// relative to, and what each URI spells, keyed by the span it shares with its
/// reference.
pub struct DartScope {
    /// The file's directory, repo-relative, without a trailing slash.
    dir: String,
    /// Each URI's form, by `(byte_start, byte_end)` of the URI node.
    uris: HashMap<(u32, u32), UriForm>,
}

/// What a URI's scheme says about where to look.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Uri<'a> {
    /// `dart:<library>` — an SDK library, outside every repository.
    Sdk(&'a str),
    /// `package:<name>/<path>` — a file in some package's `lib/`.
    Package {
        /// The package name between the scheme and the first `/`.
        name: &'a str,
        /// The path under that package's `lib/`. May be empty, which no valid
        /// Dart URI is.
        rest: &'a str,
    },
    /// No scheme: a path relative to the referring library.
    Relative(&'a str),
    /// Some other scheme — `file:`, `http:`. Named so the miss can say which.
    Other,
}

/// Split a URI on its scheme, if it has one.
///
/// A scheme is a letter followed by letters, digits, `+`, `-` or `.`, then a
/// `:` — RFC 3986's own rule. Reading it that way rather than looking for the
/// first colon is what keeps a Windows-shaped relative path from being taken
/// for a scheme.
fn scheme_of(spec: &str) -> Option<(&str, &str)> {
    let at = spec.find(':')?;
    let (scheme, rest) = spec.split_at(at);
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic()
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    Some((scheme, &rest[1..]))
}

/// What a URI names, as far as its own text can say.
fn classify(spec: &str) -> Uri<'_> {
    match scheme_of(spec) {
        Some((SDK_SCHEME, rest)) => Uri::Sdk(rest),
        Some((PACKAGE_SCHEME, rest)) => match rest.split_once('/') {
            Some((name, path)) => Uri::Package { name, rest: path },
            None => Uri::Package {
                name: rest,
                rest: "",
            },
        },
        Some(_) => Uri::Other,
        None => Uri::Relative(spec),
    }
}

/// An outcome with nothing probed.
fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: crate::Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// Join a repo-relative directory and a relative URI, resolving `.` and `..`.
///
/// `None` when the result would escape above the repository root — a real
/// `import '../../elsewhere.dart'` reaching out of the tree — and `None` for a
/// URI anchored at `/`, which is a filesystem root this scan never sees.
/// A path this scan cannot see is not one it may claim to have resolved.
fn join_path(dir: &str, spec: &str) -> Option<String> {
    if spec.starts_with('/') {
        return None;
    }
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
    let joined = parts.join("/");
    (!joined.is_empty()).then_some(joined)
}

/// The identity of the library a repo-relative path is.
fn library_id(path: &str) -> NodeId {
    node_id(Domain::Dart, &library_fqn(path))
}

/// Probe one repo-relative path, and say what a miss means.
///
/// The lookup is complete by construction — every `.dart` file the walk
/// reached is a library node — so a literal that named none of them really did
/// name no module in this repository.
fn probe_path(path: &str, probe: &dyn SymbolProbe) -> Resolution {
    let id = library_id(path);
    let outcome = if probe.probe(&id).is_some() {
        crate::Outcome::Resolved(id)
    } else {
        crate::Outcome::Unresolved(UnresolvedReason::ModuleNotFound)
    };
    Resolution {
        outcome,
        candidates: vec![id],
    }
}

/// Dart's resolver. Stateless: everything it reads is in the config, the
/// scope, or the probe.
pub struct DartResolver;

impl DartResolver {
    /// One URI, one outcome.
    fn uri(
        cfg: &DartProject,
        scope: &DartScope,
        spec: &str,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        match classify(spec) {
            Uri::Sdk(library) => Resolution {
                outcome: crate::Outcome::External(format!("{SDK_SCHEME}:{library}")),
                candidates: Vec::new(),
            },
            Uri::Package { name, rest } => Self::package(cfg, name, rest, probe),
            Uri::Relative(path) => match join_path(&scope.dir, path) {
                Some(joined) => probe_path(&joined, probe),
                None => unresolved(UnresolvedReason::ModuleNotFound),
            },
            // `file:`, `http:` and every other scheme name something no walk
            // of this repository reached, and this build holds no index of
            // what is behind one.
            Uri::Other => unresolved(UnresolvedReason::UnknownPackage),
        }
    }

    /// `package:<name>/<path>`: every name this repository *contains* first —
    /// its own package, then any dependency a `path:` places inside the tree —
    /// so each is a lookup that can miss rather than an `External` that cannot.
    fn package(cfg: &DartProject, name: &str, rest: &str, probe: &dyn SymbolProbe) -> Resolution {
        // This repository's own package, tested before anything else, so a
        // self-reference is an in-repository lookup that can miss.
        if cfg.package.as_deref() == Some(name) {
            return match cfg.own_package_path(name, rest) {
                Some(path) => probe_path(&path, probe),
                // `package:<us>` with no path names no library at all — not a
                // package outside this repository, which is what the other
                // reasons here would claim.
                None => unresolved(UnresolvedReason::ModuleNotFound),
            };
        }
        match cfg.dep(name) {
            // A `path:` says the package is a directory of this repository,
            // whose `.dart` files the walk already reached and stored — so
            // this is an in-repository lookup, and a miss under it is a miss.
            Some(DartDep::Local(dir)) => {
                // The directory is stated relative to the manifest, and the
                // manifest this build reads is the one at the root.
                let Some(base) = join_path("", &format!("{dir}/{LIB}")) else {
                    // A `path:` climbing above the repository root, or
                    // anchored at a filesystem one: a directory this scan
                    // never walked and cannot see into. Arthron's own gap,
                    // counted against the rate rather than waved through —
                    // nothing here proves no in-repository file is behind it.
                    return unresolved(UnresolvedReason::ProjectLayoutUnknown);
                };
                match join_path(&base, rest) {
                    Some(path) => probe_path(&path, probe),
                    None => unresolved(UnresolvedReason::ModuleNotFound),
                }
            }
            Some(DartDep::External) => Resolution {
                outcome: crate::Outcome::External(name.to_string()),
                candidates: Vec::new(),
            },
            // Without a manifest nothing in the tree says which package this
            // repository is, so the failure is arthron's own inference rather
            // than a statement about the name — which is a different fact from
            // a name the manifest was read and did not declare.
            None => unresolved(if cfg.package.is_none() {
                UnresolvedReason::ProjectLayoutUnknown
            } else {
                UnresolvedReason::UnknownPackage
            }),
        }
    }
}

impl Resolver<DartLang> for DartResolver {
    fn config(&self, root: &Path, _files: &FileIndex) -> Result<DartProject, LayoutError> {
        layout(root)
    }

    fn config_digest(&self, cfg: &DartProject) -> Vec<u8> {
        // The package name roots every `package:` URI in the tree, so a scan
        // under a different one describes a different graph and cannot be
        // patched into this one file by file.
        cfg.digest()
    }

    fn declared_container(
        &self,
        _cfg: &DartProject,
        _header: &DartHeader,
    ) -> Option<(String, String)> {
        // A Dart file names no container for anybody else: the library a file
        // *is* comes from its path, and Dart has no namespace above the file
        // for one file to declare on another's behalf.
        None
    }

    fn learn_containers(&self, _cfg: &mut DartProject, _names: &HashMap<String, String>) {
        // Nothing a Dart URI names is derived from another file's source, so
        // there is nothing to learn.
    }

    fn owns_file(&self, _cfg: &DartProject, _rel_path: &str) -> bool {
        // No nested-manifest fence: a `pubspec.yaml` in a subdirectory is a
        // shape phase 0 does not read, so no file is excluded on account of
        // one — see `project` for why, and for what that costs.
        true
    }

    fn def_fqn(
        &self,
        _cfg: &DartProject,
        header: &DartHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        // The file's own library node: synthesized, at the top level, and a
        // module. Its identity is the path, because that is what a URI spells.
        if def.kind == DefKind::Module
            && def.facets.contains(DefFacets::SYNTHETIC)
            && owner.is_empty()
        {
            return Some(Fqn::new(library_fqn(&header.rel_path)));
        }
        if def.name.is_empty() {
            return None;
        }
        Some(Fqn::new(decl_fqn(&header.rel_path, owner, &def.name)))
    }

    fn index_keys(&self, _cfg: &DartProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // Every Dart node is reachable by exactly one identity: a library by
        // its path, a declaration by its library-qualified name.
        Vec::new()
    }

    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        // One Dart member, written in two halves. Two shapes, and the corpus
        // writes both:
        //
        // - **A getter and a setter of one name**, which is exactly what
        //   `DefKind::Property` means — `QueueList.length` at
        //   `lib/src/queue_list.dart:159` and `:162`.
        // - **A `final` field and an explicit setter of that name.** A final
        //   field declares only a getter, so Dart allows the setter beside it:
        //   `ListSlice.length` is `final int length;` at
        //   `lib/src/list_extensions.dart:351` and `set length(…)` at `:455`,
        //   and they are one member of one class.
        //
        // Everything else sharing an FQN is two entities: Dart forbids two
        // declarations of one name in one scope, so a collision there is real
        // and must be counted.
        //
        // The cost, stated: a library that declares one accessor *twice* —
        // which does not compile — is merged rather than reported, because a
        // second getter and a setter are the same three fields from here.
        let accessors = matches!(
            (a.kind, b.kind),
            (DefKind::Property, DefKind::Property)
                | (DefKind::Property, DefKind::Field)
                | (DefKind::Field, DefKind::Property)
        );
        accessors && a.name == b.name && a.owner == b.owner
    }

    fn scope(
        &self,
        _cfg: &DartProject,
        file: &FileFacts<DartLang>,
        _probe: &dyn SymbolProbe,
    ) -> DartScope {
        let rel = &file.header.rel_path;
        let dir = match rel.rfind('/') {
            Some(at) => rel[..at].to_string(),
            None => String::new(),
        };
        DartScope {
            dir,
            uris: file
                .header
                .uris
                .iter()
                .map(|u| ((u.span.byte_start, u.span.byte_end), u.form.clone()))
                .collect(),
        }
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // Tier 2 emits no `Inherit` reference: `class C extends B` is part of
        // `C`'s structure here and is not resolved, so there is no supertype
        // relation to build and nothing for the driver to run a phase over.
        &[]
    }

    fn resolve(
        &self,
        cfg: &DartProject,
        scope: &DartScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        match scope.uris.get(&(r.span.byte_start, r.span.byte_end)) {
            Some(UriForm::Literal(spec)) => Self::uri(cfg, scope, spec, probe),
            // A URI that could not be read as one literal, and — unreachable,
            // since the extractor emits a URI record and its reference
            // together — a reference with no record at all. Both mean the
            // same thing: this build cannot say which library is named, and it
            // will not guess one.
            Some(UriForm::Dynamic) | None => unresolved(UnresolvedReason::DynamicModuleSpecifier),
        }
    }
}

/// The Dart track's scan entry point, reading every `.dart` the walk finds.
pub fn scan_dart(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_dart_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_dart`] under a repository's include/exclude globs. What
/// [`crate::track_dart::TRACK`] holds.
pub fn scan_dart_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<DartLang>(root, db, &DartExtractor, &DartResolver, filter)
}

/// Dart's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(DartLang::LANG, Lang::Dart));
    assert!(matches!(DartLang::DOMAIN, Domain::Dart));
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeclSpace, Span};
    use crate::track_dart::extract::extract;
    use std::collections::HashSet;

    fn project(package: Option<&str>, deps: &[&str], manifest: bool) -> DartProject {
        DartProject {
            package: package.map(str::to_string),
            dependencies: deps
                .iter()
                .map(|d| ((*d).to_string(), DartDep::External))
                .collect(),
            manifest,
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

    fn header(rel: &str) -> DartHeader {
        DartHeader {
            rel_path: rel.to_string(),
            uris: Vec::new(),
        }
    }

    #[test]
    fn a_scheme_is_read_by_its_own_rule_and_a_path_is_not_one() {
        assert_eq!(classify("dart:collection"), Uri::Sdk("collection"));
        assert_eq!(
            classify("package:collection/src/a.dart"),
            Uri::Package {
                name: "collection",
                rest: "src/a.dart",
            },
        );
        assert_eq!(classify("src/a.dart"), Uri::Relative("src/a.dart"));
        assert_eq!(classify("../a.dart"), Uri::Relative("../a.dart"));
        assert_eq!(classify("file:///tmp/a.dart"), Uri::Other);
        // A colon that is not a scheme separator leaves the URI relative.
        assert_eq!(classify("9a:b.dart"), Uri::Relative("9a:b.dart"));
    }

    #[test]
    fn a_path_that_climbs_past_the_root_has_no_answer() {
        assert_eq!(join_path("lib", "../a.dart"), Some("a.dart".to_string()));
        assert_eq!(join_path("lib", "../../a.dart"), None);
        assert_eq!(join_path("", "a/b.dart"), Some("a/b.dart".to_string()));
        assert_eq!(
            join_path("lib/src", "./x.dart"),
            Some("lib/src/x.dart".to_string()),
        );
        // A filesystem-rooted URI is not a path in this repository.
        assert_eq!(join_path("lib", "/a.dart"), None);
    }

    #[test]
    fn the_library_node_is_named_by_the_path_and_a_declaration_by_its_library() {
        let cfg = project(Some("collection"), &[], true);
        let table: HashSet<NodeId> = HashSet::new();
        let library = def_of(DefKind::Module, "wrappers", DefFacets::SYNTHETIC);
        assert_eq!(
            DartResolver
                .def_fqn(
                    &cfg,
                    &header("lib/src/wrappers.dart"),
                    &[],
                    &library,
                    &table
                )
                .map(Fqn::into_string),
            Some("$lib/src/wrappers.dart".to_string()),
        );
        let class = def_of(DefKind::Type, "DelegatingList", DefFacets::EXPORTED);
        assert_eq!(
            DartResolver
                .def_fqn(&cfg, &header("lib/src/wrappers.dart"), &[], &class, &table)
                .map(Fqn::into_string),
            Some("$lib/src/wrappers.dart::DelegatingList".to_string()),
        );
        let method = def_of(DefKind::Method, "add", DefFacets::EXPORTED);
        assert_eq!(
            DartResolver
                .def_fqn(
                    &cfg,
                    &header("lib/src/wrappers.dart"),
                    &["DelegatingList".to_string()],
                    &method,
                    &table,
                )
                .map(Fqn::into_string),
            Some("$lib/src/wrappers.dart::DelegatingList.add".to_string()),
        );
    }

    #[test]
    fn the_two_halves_of_one_member_merge_and_nothing_else_does() {
        let getter = def_of(DefKind::Property, "length", DefFacets::EXPORTED);
        let setter = def_of(DefKind::Property, "length", DefFacets::EXPORTED);
        assert!(DartResolver.mergeable(&getter, &setter));
        // A `final` field declares only a getter, so Dart lets an explicit
        // setter sit beside it — `ListSlice.length` in the measured corpus.
        let field = def_of(DefKind::Field, "length", DefFacets::EXPORTED);
        assert!(DartResolver.mergeable(&field, &setter));
        assert!(DartResolver.mergeable(&setter, &field));
        // Two fields of one name do not compile, and two methods are two
        // entities the count exists to surface.
        assert!(!DartResolver.mergeable(&field, &field.clone()));
        let method = def_of(DefKind::Method, "length", DefFacets::EXPORTED);
        assert!(!DartResolver.mergeable(&method, &method.clone()));
        assert!(!DartResolver.mergeable(&method, &getter));
        // A different owner is a different member.
        let mut other = getter.clone();
        other.owner = vec!["Elsewhere".to_string()];
        assert!(!DartResolver.mergeable(&getter, &other));
    }

    #[test]
    fn every_reference_is_paired_with_a_uri_record() {
        // The pairing is by span, so a reference the scope cannot find would
        // silently become `DynamicModuleSpecifier` for a perfectly literal
        // URI. It must be total, including for a directive naming two URIs.
        let cfg = project(Some("collection"), &["test"], true);
        let table: HashSet<NodeId> = HashSet::new();
        // Directive order is Dart's own: imports and exports, then parts.
        let source = "import 'dart:math';\nimport 'a.dart' if (dart.library.io) 'b.dart';\n\
                      import '${x}.dart';\nexport 'c.dart' show C;\npart 'd.dart';\n";
        let facts = extract("lib/x.dart", source);
        let scope = DartResolver.scope(&cfg, &facts, &table);
        assert_eq!(facts.refs.len(), 6, "{:?}", facts.refs);
        assert_eq!(facts.header.uris.len(), facts.refs.len());
        for r in &facts.refs {
            assert!(
                scope
                    .uris
                    .contains_key(&(r.span.byte_start, r.span.byte_end)),
                "unpaired: {}",
                r.raw_target,
            );
        }
    }
}
