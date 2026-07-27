//! The one place a Bash [`crate::Outcome`] is produced. Never drops.
//!
//! # The import model, as measured
//!
//! `source` — and `.`, which is the same builtin spelled the POSIX way — is
//! the whole of it, and bash's own lookup has three branches this mirrors:
//!
//! 1. **A literal carrying a `/`** is used as written, relative to the
//!    **working directory**. Not to the sourcing file's directory: bash does
//!    not look there, and probing it would resolve references the shell
//!    itself would not. The repository root is the only working directory a
//!    scan can name, so it is the one anchor, and a path that climbs above it
//!    is outside the repository by construction.
//! 2. **A literal carrying no `/`** is searched for on `$PATH` first, and
//!    read from the working directory when `$PATH` does not supply it. `$PATH`
//!    is an environment variable, not a repository fact; the root is probed
//!    and the miss says the name belongs to something this build did not
//!    index.
//! 3. **A specifier that is not a literal** resolves against nothing.
//!    [`UnresolvedReason::DynamicModuleSpecifier`], never a guess.
//!
//! # Why the corpus's composed paths are not matched by their tails
//!
//! The measured corpus writes twenty-five `source` lines of the shape
//! `source "$BATS_ROOT/$BATS_LIBDIR/bats-core/<name>.bash"` — three of them
//! in the twelve files this track owns — and the tail of every one really
//! does name a file in the tree. Matching on that tail would take this
//! track's rate from 0% to 50% in one commit, and it would be a guess about
//! two variables the running program computes: `BATS_ROOT` is derived at run
//! time from the resolved path of `$0`, and `BATS_LIBDIR` is an environment
//! variable with a default. A tail that matches is not a target that was
//! named. So those three, and the three pure run-time values beside them,
//! are `DynamicModuleSpecifier` with nothing probed, and the rate says what
//! that costs.
//!
//! # Why nothing here is `External`
//!
//! Bash has no manifest, so no repository *declares* that a name comes from
//! outside it. `External` sits outside **both** terms of the resolution rate,
//! which makes minting one the cheapest way there is to raise a rate with
//! nothing linked; a track that mints none cannot raise its rate by
//! reclassifying. Every path that leaves the repository is
//! [`UnresolvedReason::UnknownPackage`] instead, and counts *against* the
//! rate.
//!
//! # `LocalBinding` does not apply here
//!
//! Tier 2 emits no expression-level reference, so no Bash reference can name
//! a parameter or a local. The bucket stays empty, and the baseline records
//! it as zero — which makes this track's rate un-gameable by the one
//! reclassification the rate's own definition permits.

use std::collections::HashMap;
use std::path::Path;

use crate::UnresolvedReason;
use crate::lang::{FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe};
use crate::model::{
    DefFacets, DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, Reference, node_id,
};
use crate::track_bash::extract::{BashExtractor, BashHeader, SourceForm};
use crate::track_bash::lang::{BashLang, BashProject, function_fqn, script_fqn};

/// One file's view of what its own `source` clauses mean.
///
/// One fact and no more: what each clause spells, keyed by the span the
/// clause shares with its reference. The file's own directory is deliberately
/// **not** here — bash resolves against the working directory, so knowing
/// where the sourcing file sits would only tempt a rule the shell does not
/// have.
pub struct BashScope {
    /// Each clause's form, by `(byte_start, byte_end)` of its command.
    sources: HashMap<(u32, u32), SourceForm>,
}

/// An outcome with nothing probed.
fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: crate::Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// A repo-relative path with `.` and `..` resolved.
///
/// `None` when the result would climb above the repository root — a real
/// `source ../../elsewhere.sh` reaching out of the tree — because a path this
/// scan cannot see is not one it may claim to have resolved.
fn normalize(spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
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

/// The identity of the script a repo-relative path is.
fn script_id(path: &str) -> NodeId {
    node_id(Domain::Shell, &script_fqn(path))
}

/// Bash's resolver. Stateless: everything it reads is in the scope or the
/// probe.
pub struct BashResolver;

impl BashResolver {
    /// One literal specifier, against the one anchor a scan can name.
    fn literal(spec: &str, probe: &dyn SymbolProbe) -> Resolution {
        // `source ""` is a runtime error in bash and names no file here.
        if spec.is_empty() {
            return unresolved(UnresolvedReason::ModuleNotFound);
        }
        // An absolute path is outside this repository by construction, so
        // nothing in the tree can be probed for it.
        if spec.starts_with('/') {
            return unresolved(UnresolvedReason::UnknownPackage);
        }
        let Some(path) = normalize(spec) else {
            // Climbs above the root: outside the repository, same as above.
            return unresolved(UnresolvedReason::UnknownPackage);
        };
        let id = script_id(&path);
        if probe.probe(&id).is_some() {
            return Resolution {
                outcome: crate::Outcome::Resolved(id),
                candidates: vec![id],
            };
        }
        // The miss means two different things, and they are two reasons.
        let reason = if spec.contains('/') {
            // The path was used as written and the lookup was complete for
            // the extensions this track owns; the literal named none of them.
            UnresolvedReason::ModuleNotFound
        } else {
            // Bash searches `$PATH` before the working directory, and `$PATH`
            // is not a repository fact and was not indexed.
            UnresolvedReason::UnknownPackage
        };
        Resolution {
            outcome: crate::Outcome::Unresolved(reason),
            candidates: vec![id],
        }
    }
}

impl Resolver<BashLang> for BashResolver {
    fn config(&self, _root: &Path, _files: &FileIndex) -> Result<BashProject, LayoutError> {
        // Nothing to read: bash states no layout anywhere outside its source.
        Ok(BashProject)
    }

    fn config_digest(&self, _cfg: &BashProject) -> Vec<u8> {
        // A language with no project manifest returns an empty fingerprint
        // and is never invalidated by one.
        Vec::new()
    }

    fn declared_container(
        &self,
        _cfg: &BashProject,
        _header: &BashHeader,
    ) -> Option<(String, String)> {
        // A bash file names no container for anybody else: the script it *is*
        // comes from its path rather than from its source.
        None
    }

    fn learn_containers(&self, _cfg: &mut BashProject, _names: &HashMap<String, String>) {
        // Nothing a Bash reference binds is derived from another file's
        // source, so there is nothing to learn.
    }

    fn owns_file(&self, _cfg: &BashProject, _rel_path: &str) -> bool {
        // No nested-manifest fence: there is no manifest.
        true
    }

    fn def_fqn(
        &self,
        _cfg: &BashProject,
        header: &BashHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        // The file's own script node: synthesized, at the top level, and a
        // module. Its identity is the path, because that is what a `source`
        // spells.
        if def.kind == DefKind::Module
            && def.facets.contains(DefFacets::SYNTHETIC)
            && owner.is_empty()
        {
            return Some(Fqn::new(script_fqn(&header.rel_path)));
        }
        if def.name.is_empty() {
            return None;
        }
        Some(Fqn::new(function_fqn(&header.rel_path, owner, &def.name)))
    }

    fn index_keys(&self, _cfg: &BashProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // Every Bash node is reachable by exactly one identity: a script by
        // its path, a function by its file and chain.
        Vec::new()
    }

    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        // Asked only about a function, since the script node is a package
        // node. Writing `usage()` twice in one file is legal bash: the second
        // definition replaces the first and one function is left, so the two
        // records are that one entity. Two files writing it are already two
        // identities — the FQN carries the file — so this never merges across
        // one.
        a.kind == b.kind && a.name == b.name && a.owner == b.owner
    }

    fn scope(
        &self,
        _cfg: &BashProject,
        file: &FileFacts<BashLang>,
        _probe: &dyn SymbolProbe,
    ) -> BashScope {
        BashScope {
            sources: file
                .header
                .sources
                .iter()
                .map(|s| ((s.span.byte_start, s.span.byte_end), s.form.clone()))
                .collect(),
        }
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // Bash has no supertype relation and this track emits no `Inherit`
        // reference, so there is nothing for the driver to run a phase over.
        &[]
    }

    fn resolve(
        &self,
        _cfg: &BashProject,
        scope: &BashScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        match scope.sources.get(&(r.span.byte_start, r.span.byte_end)) {
            Some(SourceForm::Literal(spec)) => Self::literal(spec, probe),
            // A clause the shell would expand, and — unreachable, since the
            // extractor emits a clause and its reference together — a
            // reference with no clause at all. Both mean the same thing: this
            // build cannot say which file is named, and it will not guess one.
            Some(SourceForm::Dynamic) | None => {
                unresolved(UnresolvedReason::DynamicModuleSpecifier)
            }
        }
    }
}

/// The Bash track's scan entry point, reading every `.sh` and `.bash` the
/// walk finds.
pub fn scan_bash(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_bash_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_bash`] under a repository's include/exclude globs. What
/// [`crate::track_bash::TRACK`] holds.
pub fn scan_bash_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<BashLang>(root, db, &BashExtractor, &BashResolver, filter)
}

/// Bash's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(BashLang::LANG, Lang::Bash));
    assert!(matches!(BashLang::DOMAIN, Domain::Shell));
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track_bash::extract::extract;
    use std::collections::HashSet;

    #[test]
    fn a_path_that_climbs_past_the_root_has_no_answer() {
        assert_eq!(normalize("lib/../a.sh"), Some("a.sh".to_string()));
        assert_eq!(normalize("./a.sh"), Some("a.sh".to_string()));
        assert_eq!(normalize("../a.sh"), None);
        assert_eq!(normalize("a//b.sh"), Some("a/b.sh".to_string()));
    }

    #[test]
    fn every_source_reference_is_paired_with_a_clause() {
        // The pairing is by span, so a reference the scope cannot find would
        // silently become `DynamicModuleSpecifier` for a perfectly literal
        // specifier. It must be total.
        let table: HashSet<NodeId> = HashSet::new();
        let source = "source lib/a.bash\nf() {\n  source \"$x\"\n}\n. 'lib/b.bash'\n";
        let facts = extract("lib/util.bash", source);
        let scope = BashResolver.scope(&BashProject, &facts, &table);
        assert_eq!(facts.refs.len(), 3);
        for r in &facts.refs {
            assert!(
                scope
                    .sources
                    .contains_key(&(r.span.byte_start, r.span.byte_end)),
                "unpaired: {}",
                r.raw_target,
            );
        }
    }
}
