//! The one place a C++ [`crate::Outcome`] is produced. Never drops.
//!
//! # The import model, as measured
//!
//! One syntax family, two roots, and the roots are what the syntax picks
//! between:
//!
//! 1. **`#include "…"`** is resolved against the **including file's own
//!    directory** first, then against each include root in order. Both halves
//!    are real and neither is the majority: across all 55 files of the corpus
//!    50 of the 143 quoted directives are bare sibling names
//!    (`"gtest-extra.h"`, `"util.h"`) and 93 are root-relative
//!    (`"fmt/ranges.h"`). A resolver that committed to one rule would be wrong
//!    about the other set.
//! 2. **`#include <…>`** is resolved against the include roots only. It never
//!    starts at the including file, which is the whole difference the two
//!    spellings carry.
//! 3. **A C++20 `import <name>;`** names a module and no path at all. It is
//!    the one reference in this track with no directory in it.
//! 4. **`#include SOME_MACRO`** resolves against nothing.
//!    [`UnresolvedReason::DynamicModuleSpecifier`], never a guess: only a
//!    preprocessor run knows what the macro expands to.
//!
//! # What is `External`, and what is a floor
//!
//! `External` sits outside **both** terms of the resolution rate, so widening
//! it is the cheapest way there is to raise a rate with nothing linked. This
//! track therefore spends it only where it has *positive evidence* that the
//! target is not this repository's to supply, and that evidence exists for
//! exactly one shape:
//!
//! - **An angled include that names no file under any include root.**
//!   `<vector>`, `<windows.h>`, `<sys/stat.h>`. The angle syntax means "look
//!   where the implementation looks"; the resolver has enumerated every
//!   include root this repository declares and probed the tree at every
//!   candidate path, and found nothing. The header is supplied by the
//!   toolchain or by a dependency. 154 of the 155 angled directives in the
//!   files this track reads are this, and calling them unresolved would fill
//!   the gate with the standard library.
//!
//! Everything else that misses is a floor and counts *against* the rate:
//!
//! - **An angled include that names a file that *is* under an include root**,
//!   but carries an extension this build does not parse. `<fmt/base.h>` is a
//!   file in this repository. It is not `External` — laundering an
//!   in-repository header into the bucket that sits outside the rate is
//!   exactly the failure the Rust review caught one language earlier — so it
//!   is [`UnresolvedReason::ModuleNotFound`]: a literal specifier that
//!   resolved to no *module* under this build's configured resolution, whose
//!   translation units are the extensions [`crate::model::Lang::Cpp`] claims.
//! - **A quoted include that hits nothing.** The quoted syntax says "this
//!   project's own header", so a miss is this project failing to supply what
//!   it said it supplies, not a link to somebody else. That covers both the
//!   `.h` headers this build does not parse (99 sites) and the 14
//!   `"gtest/gtest.h"` / `"gmock/gmock.h"` directives whose bundle the corpus
//!   deliberately does not vendor. The second case is the PHP track's
//!   decision applied
//!   unchanged: guzzle's 170 `use` statements naming sibling packages outside
//!   the snapshot are `ModuleNotFound` and count against the rate, because a
//!   snapshot's scope is an honest floor rather than an external link.
//! - **A module name no `export module` in this repository declares.**
//!   `import std;` names the standard library's module.
//!   [`UnresolvedReason::UnknownPackage`] — a package outside the repository
//!   that was not indexed — which is the Ruby track's answer for `require
//!   'time'`, and for the same reason: this build holds no measured
//!   standard-library set, and one written from memory is the cheapest
//!   possible way to raise a rate with nothing linked.
//!
//! # `LocalBinding` does not apply here
//!
//! Tier 2 emits no expression-level reference, so no C++ reference can name a
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
use crate::track_cpp::extract::{CppExtractor, CppHeader, IncludeForm};
use crate::track_cpp::lang::{CppLang, module_fqn, unit_fqn};
use crate::track_cpp::project::{CppProject, layout};

/// One file's view of what its own includes mean.
///
/// Two facts and no more: where the file sits, which is what a quoted
/// `#include` is relative to, and what each of its clauses spells, keyed by
/// the span the clause shares with its reference.
pub struct CppScope {
    /// The file's directory, repository-relative, without a trailing slash.
    dir: String,
    /// Each clause's form, by `(byte_start, byte_end)` of its directive.
    includes: HashMap<(u32, u32), IncludeForm>,
}

/// An outcome with nothing probed.
fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: crate::Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// Join a repository-relative directory and an include specifier, resolving
/// `.` and `..`.
///
/// `None` when the result would escape above the repository root — a real
/// `#include "../../elsewhere/x.h"` reaching out of the tree — because a path
/// this scan cannot see is not one it may claim to have resolved.
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
    let joined = parts.join("/");
    (!joined.is_empty()).then_some(joined)
}

/// The identity of the unit a repository-relative path is.
fn unit_id(path: &str) -> NodeId {
    node_id(Domain::Cxx, &unit_fqn(path))
}

/// C++'s resolver. Stateless: everything it reads is in the config, the
/// scope, or the probe.
pub struct CppResolver;

impl CppResolver {
    /// Probe a candidate list in order; the first hit wins.
    ///
    /// Returns the candidates it read either way, because a miss is what the
    /// invalidation index needs to wake this reference when the file it
    /// looked for appears.
    fn probe_paths(paths: &[String], probe: &dyn SymbolProbe) -> (Option<NodeId>, Vec<NodeId>) {
        let mut candidates = Vec::new();
        for path in paths {
            let id = unit_id(path);
            if candidates.contains(&id) {
                continue;
            }
            candidates.push(id);
            if probe.probe(&id).is_some() {
                return (Some(id), candidates);
            }
        }
        (None, candidates)
    }

    /// `#include "…"`: the including file's directory, then the include roots.
    fn quoted(
        cfg: &CppProject,
        scope: &CppScope,
        spec: &str,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let mut paths = Vec::new();
        if let Some(sibling) = join_path(&scope.dir, spec) {
            paths.push(sibling);
        }
        for root in &cfg.include_roots {
            if let Some(rooted) = join_path(root, spec) {
                paths.push(rooted);
            }
        }
        let (hit, candidates) = Self::probe_paths(&paths, probe);
        let outcome = match hit {
            Some(id) => crate::Outcome::Resolved(id),
            // The quoted syntax says this project supplies the header. A miss
            // is this project's floor, never a link to somebody else.
            None => crate::Outcome::Unresolved(UnresolvedReason::ModuleNotFound),
        };
        Resolution {
            outcome,
            candidates,
        }
    }

    /// `#include <…>`: the include roots only, and then the one question this
    /// track spends `External` on.
    fn angled(cfg: &CppProject, spec: &str, probe: &dyn SymbolProbe) -> Resolution {
        let paths: Vec<String> = cfg
            .include_roots
            .iter()
            .filter_map(|root| join_path(root, spec))
            .collect();
        let (hit, candidates) = Self::probe_paths(&paths, probe);
        if let Some(id) = hit {
            return Resolution {
                outcome: crate::Outcome::Resolved(id),
                candidates,
            };
        }
        // A file this repository really does publish on its include path,
        // under an extension this build does not parse. In-repository, and so
        // never `External`.
        let in_repo = paths.iter().any(|p| cfg.unparsed.contains(p));
        let outcome = if in_repo {
            crate::Outcome::Unresolved(UnresolvedReason::ModuleNotFound)
        } else {
            crate::Outcome::External(spec.to_string())
        };
        Resolution {
            outcome,
            candidates,
        }
    }

    /// A C++20 `import <name>;`.
    fn module(name: &str, probe: &dyn SymbolProbe) -> Resolution {
        let id = node_id(Domain::Cxx, &module_fqn(name));
        let outcome = if probe.probe(&id).is_some() {
            crate::Outcome::Resolved(id)
        } else {
            // No `export module` in this repository declares it, and this
            // build indexes no standard-library module.
            crate::Outcome::Unresolved(UnresolvedReason::UnknownPackage)
        };
        Resolution {
            outcome,
            candidates: vec![id],
        }
    }
}

impl Resolver<CppLang> for CppResolver {
    fn config(&self, root: &Path, _files: &FileIndex) -> Result<CppProject, LayoutError> {
        layout(root)
    }

    fn config_digest(&self, cfg: &CppProject) -> Vec<u8> {
        cfg.digest()
    }

    fn declared_container(
        &self,
        _cfg: &CppProject,
        _header: &CppHeader,
    ) -> Option<(String, String)> {
        // A C++ file names no container for anybody else: `namespace fmt` is
        // reopened by every file that wants it, and the unit a file *is*
        // comes from its path rather than from its source.
        None
    }

    fn learn_containers(&self, _cfg: &mut CppProject, _names: &HashMap<String, String>) {
        // Nothing a C++ include binds is derived from another file's source.
    }

    fn owns_file(&self, _cfg: &CppProject, _rel_path: &str) -> bool {
        // No nested-manifest fence: C++ has no manifest to nest.
        true
    }

    fn def_fqn(
        &self,
        _cfg: &CppProject,
        header: &CppHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        // The file's own unit node: synthesized, at the top level, and a
        // module. Its identity is the path, because that is what an
        // `#include` spells.
        if def.kind == DefKind::Module
            && def.facets.contains(DefFacets::SYNTHETIC)
            && owner.is_empty()
        {
            return Some(Fqn::new(unit_fqn(&header.rel_path)));
        }
        if def.name.is_empty() {
            return None;
        }
        // `export module fmt;`: a named module, in the one identity space a
        // namespace of the same name cannot reach.
        if def.kind == DefKind::Module
            && def.facets.contains(DefFacets::EXPORTED)
            && owner.is_empty()
        {
            return Some(Fqn::new(module_fqn(&def.name)));
        }
        let scope = owner.join("::");
        Some(Fqn::new(if scope.is_empty() {
            def.name.clone()
        } else {
            format!("{scope}::{}", def.name)
        }))
    }

    fn index_keys(&self, _cfg: &CppProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // Every C++ node here is reachable by exactly one identity: a unit by
        // its path, a module by its name, an entity by its qualified name.
        Vec::new()
    }

    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        // The one-definition rule is C++'s own: `namespace fmt` reopened in
        // thirty files, a class declared in one unit and defined in another,
        // a prototype and its body — each is one entity written more than
        // once, and counting them as collisions would bury the collision
        // counter under ordinary C++. A class and a function under one name
        // are not one entity, and that really is what the counter is for.
        //
        // The cost, recorded rather than hidden: an overload set collapses
        // into one identity. Discriminating overloads is parameter-type
        // resolution, which is exactly what tier 2 does not claim.
        a.kind == b.kind && a.name == b.name && a.owner == b.owner
    }

    fn scope(
        &self,
        _cfg: &CppProject,
        file: &FileFacts<CppLang>,
        _probe: &dyn SymbolProbe,
    ) -> CppScope {
        let rel = &file.header.rel_path;
        let dir = match rel.rfind('/') {
            Some(at) => rel[..at].to_string(),
            None => String::new(),
        };
        CppScope {
            dir,
            includes: file
                .header
                .includes
                .iter()
                .map(|i| ((i.span.byte_start, i.span.byte_end), i.form.clone()))
                .collect(),
        }
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // Tier 2 emits no `Inherit` reference: a base clause is part of the
        // derived class's structure here and is not resolved, so there is no
        // supertype relation to build and nothing for the driver to run a
        // phase over.
        &[]
    }

    fn resolve(
        &self,
        cfg: &CppProject,
        scope: &CppScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        match scope.includes.get(&(r.span.byte_start, r.span.byte_end)) {
            Some(IncludeForm::Quoted(spec)) => Self::quoted(cfg, scope, spec, probe),
            Some(IncludeForm::Angle(spec)) => Self::angled(cfg, spec, probe),
            Some(IncludeForm::Module(name)) => Self::module(name, probe),
            // A directive whose specifier is a macro, and — unreachable,
            // since the extractor emits a clause and its reference together —
            // a reference with no clause at all. Both mean the same thing:
            // this build cannot say which file is named, and it will not
            // guess one.
            Some(IncludeForm::Computed) | None => {
                unresolved(UnresolvedReason::DynamicModuleSpecifier)
            }
        }
    }
}

/// The C++ track's scan entry point, reading every file the walk finds under
/// an extension [`CppLang`] claims.
pub fn scan_cpp(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_cpp_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_cpp`] under a repository's include/exclude globs. What
/// [`crate::track_cpp::TRACK`] holds.
pub fn scan_cpp_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<CppLang>(root, db, &CppExtractor, &CppResolver, filter)
}

/// C++'s `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(CppLang::LANG, Lang::Cpp));
    assert!(matches!(CppLang::DOMAIN, Domain::Cxx));
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_that_climbs_past_the_root_has_no_answer() {
        assert_eq!(join_path("src", "../a.h"), Some("a.h".to_string()));
        assert_eq!(join_path("src", "../../a.h"), None);
        assert_eq!(join_path("", "a/b.h"), Some("a/b.h".to_string()));
        assert_eq!(join_path("test", "./x.h"), Some("test/x.h".to_string()),);
        assert_eq!(
            join_path("include", "fmt/format.h"),
            Some("include/fmt/format.h".to_string()),
        );
        // A file at the repository root, included by a sibling at the root.
        assert_eq!(join_path("", "x.h"), Some("x.h".to_string()));
    }
}
