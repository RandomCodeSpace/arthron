//! HCL's [`Language`] impl: the constants the track is reported under, the
//! three types only HCL's own layers may read, and the FQN grammar every one
//! of them agrees on.
//!
//! # The unit of resolution is a directory
//!
//! HCL has no import statement, no package clause and no qualifier. A
//! Terraform *module* is a directory, every `.tf` file in it contributes to
//! one namespace by position in the filesystem alone, and the only site that
//! names another module is a `module` block's `source` attribute holding a
//! relative path. So the container a definition lives in is the file's
//! **directory**, and it is declared by every file under it exactly as a Go
//! package is declared by every file in its directory.
//!
//! # The FQN grammar
//!
//! ```text
//! '//' <repo-relative directory>  ( '#' <address> )?
//! ```
//!
//! A container is `//` followed by the repo-relative directory path, so the
//! root module is `//` and `modules/flow-log` is `//modules/flow-log`. A
//! definition is its container, a `#`, and the **address Terraform itself
//! uses** for the thing:
//!
//! | written | address |
//! |---|---|
//! | `resource "aws_vpc" "this"` | `resource.aws_vpc.this` |
//! | `data "aws_region" "current"` | `data.aws_region.current` |
//! | `variable "vpc_cidr"` | `var.vpc_cidr` |
//! | `output "vpc_id"` | `output.vpc_id` |
//! | `module "vpc"` | `module.vpc` |
//! | `name` inside `locals` | `local.name` |
//!
//! Five of the six are Terraform's own spelling. The sixth is not: a managed
//! resource is written `aws_vpc.this` in an expression, with no prefix at
//! all, and the block keyword `resource` is put back in front of it here so
//! that the six address spaces stay disjoint — `resource "var" "x"` is
//! syntactically writable, and without the prefix it would name the same
//! thing as `variable "x"`.
//!
//! Two properties this buys:
//!
//! - **The whole path is the name.** `modules/flow-log` and
//!   `examples/flow-log` are two containers, not one, so a `source =
//!   "../../modules/flow-log"` written from `examples/flow-log` cannot bind
//!   to the directory it was written in. A resolver keyed on the last
//!   segment would mint that wrong edge, and no tally would show it.
//! - **`//` cannot be spelled by anything else.** It opens a comment in HCL,
//!   so no identifier, block type or label may contain it, and no `.tf` file
//!   can write a name that collides with a container. It also keeps
//!   [`crate::pipeline`]'s `external:` prefix unreachable: every identity in
//!   this domain starts with `//`, and none contains a `:`.
//!
//! The one crossing is injective for the same reason Go's is. A definition
//! FQN is `//<dir>#<address>`, and every address begins with one of the six
//! keywords above followed by a `.`; a container FQN is `//<dir>` and carries
//! no `#` unless a directory is *named* with one. Colliding therefore needs a
//! directory literally called `x#resource.aws_vpc.this` sitting beside a
//! directory `x` that declares that resource — which is not a tree Terraform
//! can be pointed at, and is the same bound the Go grammar accepts.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_hcl::extract::HclHeader;
use crate::track_hcl::resolve::HclScope;

/// The HCL language. Stateless; only its associated types carry anything.
pub struct HclLang;

/// Phase 0 for HCL: deliberately empty.
///
/// Every track that has a phase 0 has it because the language states a name
/// the source does not — Go's module path, Ruby's load path, PHP's PSR-4
/// prefixes. **HCL states nothing anywhere.** A module's name is where it
/// sits, a `source` is resolved against the file that wrote it, and both
/// facts are in the tree the walk already read.
///
/// The thing that looks like a manifest is not one. `versions.tf` is a `.tf`
/// file the walk reads like any other, and what it declares —
/// `required_providers`, `required_version` — decides no identity in this
/// graph: a provider is not a module, its address lives in a different
/// namespace, and nothing here resolves against it (see
/// [`crate::track_hcl::extract`] for why that is a refusal and not an
/// omission).
///
/// So the digest is empty and an HCL scan is never invalidated by a manifest
/// — which is the contract [`crate::lang::Resolver::config_digest`] already
/// states for a language with no project manifest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HclProject;

impl Language for HclLang {
    const LANG: Lang = Lang::Hcl;
    const DOMAIN: Domain = Domain::Hcl;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what HCL owns and this one cannot drift apart.
    ///
    /// `.tfvars`, `.hcl`, `.nomad` and `.tofu` are deliberately **not**
    /// claimed: the extension list was committed with the tier-2
    /// registration, and the honest moment to widen it is a commit that
    /// measures the files it adds. This one measures `.tf` alone.
    fn extensions() -> &'static [&'static str] {
        Lang::Hcl.extensions()
    }

    /// Terraform's module and provider cache. `terraform init` unpacks every
    /// remote module's **source** into `.terraform/modules/`, so descending
    /// into it would index somebody else's `.tf` files as if this repository
    /// had written them — inventing in-repository containers that a `source`
    /// could then resolve against, which inflates the resolution rate with
    /// links to code the repository does not own.
    ///
    /// Belt and braces: the walk skips hidden directories already, which is
    /// what the test beside this asserts. The name is listed anyway because
    /// the cost of the walk's default changing is a rate that rises for no
    /// reason anyone would look for.
    fn skip_dirs() -> &'static [&'static str] {
        &[".terraform"]
    }

    type Header = HclHeader;
    type Scope = HclScope;
    type Config = HclProject;
}

/// The reserved prefix every HCL identity carries, and nothing in a `.tf`
/// file can write: `//` opens a comment.
pub const DIR_MARK: &str = "//";

/// The crossing from the container namespace into the declaration namespace.
pub const CROSSING: char = '#';

/// The container FQN of a repo-relative directory: `""` → `//`,
/// `modules/flow-log` → `//modules/flow-log`.
///
/// Total, because every `.tf` file the walk reaches sits in a directory
/// whether or not it declares anything — the corpus's thirteen zero-byte
/// `variables.tf` files each declare their directory and nothing else.
pub fn module_fqn(dir: &str) -> String {
    format!("{DIR_MARK}{dir}")
}

/// The FQN of one definition: its container, the crossing, and its address.
pub fn address_fqn(dir: &str, address: &str) -> String {
    format!("{}{CROSSING}{address}", module_fqn(dir))
}

/// The directory part of a repo-relative path, without a trailing slash.
/// `""` for a file at the top of the repository.
pub fn dir_of(rel_path: &str) -> &str {
    match rel_path.rfind('/') {
        Some(at) => &rel_path[..at],
        None => "",
    }
}

/// The last segment of a repo-relative directory, which is what a Terraform
/// user calls the module. `""` for the root module, which has no name of its
/// own.
///
/// Never an identity: two directories may share a basename, and the corpus
/// has such a pair. It is the container node's *display* name only — the
/// second half of what [`crate::store::NodeRecord::Package`] stores.
pub fn dir_name(dir: &str) -> &str {
    match dir.rfind('/') {
        Some(at) => &dir[at + 1..],
        None => dir,
    }
}

/// Join a repo-relative directory and a local `source` path, resolving `.`
/// and `..`.
///
/// `None` when the result would climb above the repository root: a directory
/// this scan cannot see is not one it may claim to have found.
pub fn join_dir(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hcl_reports_as_hcl_and_hashes_in_the_hcl_domain() {
        assert_eq!(HclLang::LANG, Lang::Hcl);
        assert_eq!(HclLang::DOMAIN, Domain::Hcl);
        assert_eq!(HclLang::LANG.domain(), HclLang::DOMAIN);
        assert_eq!(HclLang::LANG.tier(), 2);
        assert_eq!(HclLang::LANG.rate_scope(), "import resolution");
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(HclLang::extensions(), Lang::Hcl.extensions());
        assert_eq!(HclLang::extensions(), ["tf"]);
        for unclaimed in ["tfvars", "hcl", "nomad", "tofu", "tf.json"] {
            assert!(!HclLang::extensions().contains(&unclaimed));
        }
    }

    #[test]
    fn the_module_cache_is_never_descended_into() {
        assert!(HclLang::skip_dirs().contains(&".terraform"));
    }

    #[test]
    fn a_container_is_its_whole_path_and_not_its_last_segment() {
        assert_eq!(module_fqn(""), "//");
        assert_eq!(module_fqn("modules/flow-log"), "//modules/flow-log");
        assert_ne!(
            module_fqn("modules/flow-log"),
            module_fqn("examples/flow-log"),
            "two directories with one basename are two containers",
        );
        assert_eq!(dir_name("modules/flow-log"), "flow-log");
        assert_eq!(dir_name(""), "");
    }

    #[test]
    fn the_crossing_appears_once_and_only_below_a_container() {
        assert_eq!(address_fqn("", "var.cidr"), "//#var.cidr");
        assert_eq!(
            address_fqn("modules/flow-log", "resource.aws_flow_log.this"),
            "//modules/flow-log#resource.aws_flow_log.this",
        );
        assert_eq!(
            address_fqn("", "var.cidr").matches(CROSSING).count(),
            1,
            "a definition FQN carries exactly one crossing",
        );
        assert!(!module_fqn("modules/flow-log").contains(CROSSING));
    }

    #[test]
    fn every_identity_starts_at_a_name_a_tf_file_cannot_write() {
        // `//` opens a comment in HCL, so no block type, label or identifier
        // can spell it — and `external:` stays unreachable because no
        // identity here contains a colon.
        for fqn in [
            module_fqn(""),
            module_fqn("examples/simple"),
            address_fqn("examples/simple", "module.vpc"),
        ] {
            assert!(fqn.starts_with(DIR_MARK), "{fqn}");
            assert!(!fqn.contains(':'), "{fqn}");
        }
    }

    #[test]
    fn a_files_directory_is_the_path_above_it() {
        assert_eq!(dir_of("examples/simple/main.tf"), "examples/simple");
        assert_eq!(dir_of("main.tf"), "");
        assert_eq!(dir_of(""), "");
    }

    #[test]
    fn a_path_that_climbs_past_the_root_has_no_answer() {
        assert_eq!(join_dir("examples/simple", "../../"), Some(String::new()));
        assert_eq!(join_dir("examples/simple", "../.."), Some(String::new()));
        assert_eq!(
            join_dir("examples/flow-log", "../../modules/flow-log"),
            Some("modules/flow-log".to_string()),
        );
        assert_eq!(join_dir("", "./child"), Some("child".to_string()));
        assert_eq!(
            join_dir("examples/simple", ".././simple"),
            Some("examples/simple".to_string())
        );
        assert_eq!(join_dir("", "../x"), None);
        assert_eq!(join_dir("examples", "../../x"), None);
    }
}
