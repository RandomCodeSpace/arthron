//! The one place an HCL [`crate::Outcome`] is produced. Never drops.
//!
//! # The import model, as measured
//!
//! **HCL has no import statement.** The only site in a `.tf` file that names
//! something declared elsewhere is a `module` block's `source` attribute, and
//! what it names is a **directory** — every `.tf` file in it at once, the way
//! a Go import path names a package rather than a file. So there are three
//! rules and each is a rule about *what kind of thing the text is*:
//!
//! 1. **A local path** — one that begins with `./` or `../`, which is
//!    Terraform's own rule and the only form it reads from disk — is joined
//!    to the directory of the file that wrote it and probed as a container.
//!    A hit is [`crate::Outcome::Resolved`]; a miss is
//!    [`crate::UnresolvedReason::ModuleNotFound`], and so is a path that
//!    climbs above the repository root.
//! 2. **A package address Terraform can fetch** — a registry address, a
//!    GitHub or Bitbucket shorthand, a forced source type (`git::`, `hg::`,
//!    `s3::`, `gcs::`) or a URL — is [`crate::Outcome::External`], named by
//!    the package part of the address. None of them is ever on disk: `terraform
//!    init` unpacks them into `.terraform/modules/`, which this track's walk
//!    never descends into.
//! 3. **A source that is not a literal** resolves against nothing.
//!    [`crate::UnresolvedReason::DynamicModuleSpecifier`], never a guess.
//!
//! # Rule 2 is a grammar, not a fallback, and that is the anti-laundering
//!
//! `External` sits outside **both** terms of the resolution rate, so the
//! cheapest way there is to raise a rate without linking anything is to widen
//! it. The obvious rule here — "not a local path, therefore external" — is
//! exactly that widening: `source = "modules/flow-log"` names a directory
//! this repository really does contain, Terraform will not read it from disk,
//! and calling it external would move a reference this repository wrote
//! outside the measurement entirely.
//!
//! So rule 2 asks whether Terraform's own address grammar says the text names
//! a *package*, and anything that is neither a path it reads nor a package it
//! fetches is [`crate::UnresolvedReason::ModuleNotFound`] and **counts
//! against the rate**. Where a rule cannot tell the two apart, the answer
//! that costs the rate is the only one that cannot launder a miss. The
//! `external` count is a baseline field besides, so any drift in it fails the
//! gate and has to be re-based deliberately.
//!
//! # Why a local miss is `ModuleNotFound` and never `UnknownPackage`
//!
//! A local path names this repository by construction — it is relative to a
//! file the walk just read. The lookup is complete: every directory holding a
//! `.tf` file the walk reached declares a container, so a literal that names
//! none of them named a directory that is not there, or is there and holds no
//! `.tf` file. That is the reason's own definition: the specifier is a
//! literal and resolved to no module under the configured resolution.
//!
//! # `LocalBinding` does not apply here
//!
//! Tier 2 emits no expression-level reference, so no HCL reference can name a
//! parameter, a local or a receiver — and HCL has none of those in the sense
//! the bucket means. It stays empty, and the baseline records it as zero,
//! which makes this rate un-gameable by the one reclassification the rate's
//! own definition permits.
//!
//! # Known limits, recorded rather than left to be rediscovered
//!
//! - **A source spelled `..` or `.` exactly** is not a local path under
//!   Terraform's documented rule ("must begin with `./` or `../`"), and is no
//!   package address either, so it lands in `ModuleNotFound`. That is the
//!   conservative direction: the corpus writes `../..` and `../../`, both of
//!   which begin with `../`.
//! - **A sub-directory of a remote package** — `ns/name/provider//modules/x`
//!   — is external under the package part alone. What the sub-directory holds
//!   is inside somebody else's package and is not a node this graph has.
//! - **`.tf.json` is not read.** The JSON syntax for HCL is a different
//!   surface with a different grammar, and the extension list this track
//!   claims stops at `.tf`.

use std::collections::HashMap;
use std::path::Path;

use crate::lang::{
    Extractor, FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe,
};
use crate::model::{DefKind, Definition, Domain, Fqn, NodeId, RefKind, Reference, node_id};
use crate::track_hcl::extract::{HclExtractor, HclHeader, SourceForm};
use crate::track_hcl::lang::{HclLang, HclProject, address_fqn, dir_of, join_dir, module_fqn};
use crate::{Outcome, UnresolvedReason};

/// One file's view of what its own module sources mean.
///
/// Two facts and no more: where the file sits, which is what a local path is
/// relative to, and what each `source` attribute spells, keyed by the span it
/// shares with its reference.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HclScope {
    /// The file's directory, repo-relative, without a trailing slash.
    dir: String,
    /// Each `source` attribute's form, by `(byte_start, byte_end)`.
    sources: HashMap<(u32, u32), SourceForm>,
}

/// An outcome with nothing probed.
fn unresolved(reason: UnresolvedReason) -> Resolution {
    Resolution {
        outcome: Outcome::Unresolved(reason),
        candidates: Vec::new(),
    }
}

/// Whether a `source` is a local path: Terraform reads a source from disk if
/// and only if it begins with `./` or `../`.
fn is_local_path(spec: &str) -> bool {
    spec.starts_with("./") || spec.starts_with("../")
}

/// The package part of a source address, when Terraform's own grammar says
/// the text names a package it can fetch.
///
/// `None` for anything that is neither a package nor a path — which is the
/// answer that keeps rule 2 from laundering a miss. See the module docs.
fn remote_package(spec: &str) -> Option<&str> {
    let package = &spec[..subdir_at(spec).unwrap_or(spec.len())];
    if package.is_empty() {
        return None;
    }
    // A forced source type: `git::`, `hg::`, `s3::`, `gcs::`.
    if package.contains("::") {
        return Some(package);
    }
    // A URL, or git's scp-like syntax.
    if ["http://", "https://", "ssh://", "git@"]
        .iter()
        .any(|scheme| package.starts_with(scheme))
    {
        return Some(package);
    }
    // A registry address — `<namespace>/<name>/<provider>` — or the same with
    // a host in front, which is also the shape of the GitHub and Bitbucket
    // shorthands.
    let parts: Vec<&str> = package.split('/').collect();
    let registry = matches!(parts.len(), 3 | 4) && parts.iter().all(|part| !part.is_empty());
    registry.then_some(package)
}

/// Where a source address's `//<subdir>` suffix starts, if it has one.
///
/// A `//` that follows a `:` is a URL's scheme separator and not the
/// sub-directory marker — `git::https://example.com/vpc.git` is one package
/// and no sub-directory.
fn subdir_at(spec: &str) -> Option<usize> {
    spec.match_indices("//")
        .find(|(at, _)| *at == 0 || !spec[..*at].ends_with(':'))
        .map(|(at, _)| at)
}

/// HCL's resolver. Stateless: everything it reads is in the scope or the
/// probe.
pub struct HclResolver;

impl HclResolver {
    /// A `source` that is one plain string.
    fn literal(scope: &HclScope, spec: &str, probe: &dyn SymbolProbe) -> Resolution {
        if is_local_path(spec) {
            // Rule 1. A local path names this repository by construction, so
            // it can never be `External` however badly it misses.
            let Some(dir) = join_dir(&scope.dir, spec) else {
                return unresolved(UnresolvedReason::ModuleNotFound);
            };
            let id = node_id(Domain::Hcl, &module_fqn(&dir));
            let outcome = if probe.probe(&id).is_some() {
                Outcome::Resolved(id)
            } else {
                Outcome::Unresolved(UnresolvedReason::ModuleNotFound)
            };
            return Resolution {
                outcome,
                candidates: vec![id],
            };
        }
        match remote_package(spec) {
            // Rule 2.
            Some(package) => Resolution {
                outcome: Outcome::External(package.to_string()),
                candidates: Vec::new(),
            },
            // Neither a path Terraform reads nor a package it fetches.
            None => unresolved(UnresolvedReason::ModuleNotFound),
        }
    }
}

impl Resolver<HclLang> for HclResolver {
    /// Phase 0 reads nothing. See [`HclProject`].
    fn config(&self, _root: &Path, _files: &FileIndex) -> Result<HclProject, LayoutError> {
        Ok(HclProject)
    }

    /// Empty: no manifest decides any identity here, so no manifest can
    /// invalidate a store.
    fn config_digest(&self, _cfg: &HclProject) -> Vec<u8> {
        Vec::new()
    }

    /// `None`. A container's identity is its directory, which the file's own
    /// path states — both phases build it from the same bytes, and there is
    /// nothing another file could teach either of them.
    fn declared_container(
        &self,
        _cfg: &HclProject,
        _header: &HclHeader,
    ) -> Option<(String, String)> {
        None
    }

    /// Nothing to learn, for the reason [`Resolver::declared_container`]
    /// gives.
    fn learn_containers(&self, _cfg: &mut HclProject, _names: &HashMap<String, String>) {}

    /// Every `.tf` file the walk reached. There is no nested-manifest fence:
    /// HCL has no manifest, and a directory below another is a module of its
    /// own rather than a part of it — which is exactly what makes a `module`
    /// block's `source` a reference between two of them.
    fn owns_file(&self, _cfg: &HclProject, _rel_path: &str) -> bool {
        true
    }

    fn def_fqn(
        &self,
        _cfg: &HclProject,
        header: &HclHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        let dir = dir_of(&header.rel_path);
        // The file's own container: the directory, whose identity is its
        // repo-relative path. Every `.tf` file under it declares it, which is
        // what a Terraform module is.
        if def.kind == DefKind::Module {
            return Some(Fqn::new(module_fqn(dir)));
        }
        // Everything else carries its address prefix in `owner`, so an
        // encloser — which arrives as a synthetic definition holding the same
        // path — spells the identity the definition phase filed.
        if owner.is_empty() || def.name.is_empty() {
            return None;
        }
        let address = format!("{}.{}", owner.join("."), def.name);
        Some(Fqn::new(address_fqn(dir, &address)))
    }

    /// Empty: every HCL node is reachable by exactly one identity — a
    /// directory by its path, a declaration by its address in that directory.
    /// HCL has no alias, no re-export and no overload set.
    fn index_keys(&self, _cfg: &HclProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        Vec::new()
    }

    /// Never. Terraform rejects two declarations of one address in one
    /// module — two `variable "x"` blocks, or a `resource` pair with one
    /// type and one name, are a hard error whichever files they sit in — so
    /// a shared identity here is a genuine collision and the report must say
    /// so. A directory is the one record several files declare without that
    /// being one, and it is a package node, which this question is never
    /// asked about.
    fn mergeable(&self, _a: &Definition, _b: &Definition) -> bool {
        false
    }

    fn scope(
        &self,
        _cfg: &HclProject,
        file: &FileFacts<HclLang>,
        _probe: &dyn SymbolProbe,
    ) -> HclScope {
        HclScope {
            dir: dir_of(&file.header.rel_path).to_string(),
            sources: file
                .header
                .sources
                .iter()
                .map(|s| ((s.span.byte_start, s.span.byte_end), s.form.clone()))
                .collect(),
        }
    }

    /// Empty. Tier 2 emits no inheritance reference, and HCL has no
    /// inheritance to emit one for.
    fn link_kinds(&self) -> &'static [RefKind] {
        &[]
    }

    fn resolve(
        &self,
        _cfg: &HclProject,
        scope: &HclScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        match scope.sources.get(&(r.span.byte_start, r.span.byte_end)) {
            Some(SourceForm::Literal(spec)) => Self::literal(scope, spec, probe),
            // A source this build cannot read as one literal, and —
            // unreachable, since the extractor emits an attribute and its
            // reference together — a reference with no attribute at all. Both
            // mean the same thing: this build cannot say which module is
            // named, and it will not guess one.
            Some(SourceForm::Dynamic) | None => {
                unresolved(UnresolvedReason::DynamicModuleSpecifier)
            }
        }
    }
}

/// The HCL track's scan entry point, reading every `.tf` the walk finds.
pub fn scan_hcl(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_hcl_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_hcl`] under a repository's include/exclude globs. What
/// [`crate::track_hcl::TRACK`] holds.
pub fn scan_hcl_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<HclLang>(root, db, &HclExtractor, &HclResolver, filter)
}

/// HCL's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(HclLang::LANG, crate::model::Lang::Hcl));
    assert!(matches!(HclLang::DOMAIN, Domain::Hcl));
};

/// The extractor's `Extractor` impl is what the driver runs;
/// [`crate::track_hcl::extract::extract`] is what the fixtures call. Naming
/// both keeps the trait object honest.
const _: fn() = || {
    fn assert_extractor<T: Extractor<HclLang>>() {}
    assert_extractor::<HclExtractor>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_path_is_the_only_form_terraform_reads_from_disk() {
        assert!(is_local_path("./child"));
        assert!(is_local_path("../.."));
        assert!(is_local_path("../../modules/flow-log"));
        // The documented rule is "begins with `./` or `../`", and these do
        // not — the conservative direction, since neither is a package
        // either and both therefore count against the rate.
        assert!(!is_local_path(".."));
        assert!(!is_local_path("."));
        assert!(!is_local_path("modules/flow-log"));
        assert!(!is_local_path(""));
    }

    #[test]
    fn a_scheme_separator_is_not_a_subdirectory_marker() {
        assert_eq!(subdir_at("git::https://example.com/vpc.git"), None);
        assert_eq!(subdir_at("https://example.com/vpc.zip"), None);
        assert_eq!(subdir_at("ns/name/provider//modules/x"), Some(16));
        assert_eq!(subdir_at("//x"), Some(0));
        assert_eq!(subdir_at("ns/name/provider"), None);
    }

    #[test]
    fn only_an_address_terraform_can_fetch_is_a_package() {
        for package in [
            "terraform-aws-modules/s3-bucket/aws",
            "app.terraform.io/example-corp/vpc/aws",
            "github.com/hashicorp/example",
            "git::https://example.com/vpc.git",
            "https://example.com/vpc.zip",
            "git@github.com:hashicorp/example.git",
        ] {
            assert_eq!(remote_package(package), Some(package), "{package}");
        }
        assert_eq!(
            remote_package("terraform-aws-modules/vpc/aws//modules/x"),
            Some("terraform-aws-modules/vpc/aws"),
        );
        // Neither a path Terraform reads nor a package it fetches. Every one
        // of these counts against the rate rather than leaving it.
        for neither in [
            "",
            "vpc",
            "modules/flow-log",
            "a/b/",
            "a//b",
            "..",
            "/abs/path",
        ] {
            assert_eq!(remote_package(neither), None, "{neither}");
        }
    }
}
