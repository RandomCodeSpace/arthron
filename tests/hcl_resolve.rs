//! Resolver fixtures for HCL: one import model, one outcome per reference.
//!
//! The model these pin is the one the corpus provenance measured: HCL has no
//! import statement, and the only site that names something declared
//! elsewhere is a `module` block's `source`. What it names is a **directory**
//! — every `.tf` file in it at once — which is why the target of the edge is
//! the container node a directory declares, the way Go's package node is
//! declared by every file in its directory.

use std::collections::HashSet;

use arthron::Outcome;
use arthron::UnresolvedReason::{self, *};
use arthron::lang::Resolver;
use arthron::model::{Domain, NodeId, node_id};
use arthron::track_hcl::extract::extract;
use arthron::track_hcl::lang::{HclProject, module_fqn};
use arthron::track_hcl::resolve::HclResolver;

/// A symbol table holding one container node per repo-relative directory.
fn table(dirs: &[&str]) -> HashSet<NodeId> {
    dirs.iter()
        .map(|d| node_id(Domain::Hcl, &module_fqn(d)))
        .collect()
}

/// Every reference in one file, resolved against a set of known directories.
fn outcomes(rel: &str, source: &str, dirs: &[&str]) -> Vec<Outcome<NodeId, String>> {
    let cfg = HclProject;
    let known = table(dirs);
    let facts = extract(rel, source);
    let scope = HclResolver.scope(&cfg, &facts, &known);
    facts
        .refs
        .iter()
        .map(|r| HclResolver.resolve(&cfg, &scope, r, &known).outcome)
        .collect()
}

/// The single outcome of a one-module file whose `source` is `spec`.
fn only(rel: &str, spec: &str, dirs: &[&str]) -> Outcome<NodeId, String> {
    let source = format!("module \"m\" {{\n  source = \"{spec}\"\n}}\n");
    let mut got = outcomes(rel, &source, dirs);
    assert_eq!(got.len(), 1, "{got:?}");
    got.remove(0)
}

fn resolved_to(o: &Outcome<NodeId, String>, dir: &str) -> bool {
    *o == Outcome::Resolved(node_id(Domain::Hcl, &module_fqn(dir)))
}

fn reason(o: &Outcome<NodeId, String>) -> Option<&UnresolvedReason> {
    o.unresolved_reason()
}

// ---------------------------------------------------------------------------
// A local path names a directory, and the directory is the whole of it
// ---------------------------------------------------------------------------

#[test]
fn a_local_path_resolves_against_the_calling_files_directory() {
    let got = only(
        "examples/simple/main.tf",
        "./child",
        &["examples/simple/child"],
    );
    assert!(resolved_to(&got, "examples/simple/child"), "{got:?}");
}

#[test]
fn a_path_that_climbs_to_the_repository_root_names_the_root_module() {
    // The corpus writes both spellings, twelve times and four times: the
    // trailing slash is not part of the name.
    for spec in ["../../", "../.."] {
        let got = only("examples/simple/main.tf", spec, &[""]);
        assert!(resolved_to(&got, ""), "{spec}: {got:?}");
    }
}

#[test]
fn the_whole_path_names_the_directory_and_not_its_last_segment() {
    // The discriminating case, and the corpus has it: `modules/flow-log` and
    // `examples/flow-log` are two directories with one basename. A resolver
    // keyed on the last segment binds the caller to itself — a wrong edge,
    // which is worse than a miss, and nothing about the tally would say so.
    let got = only(
        "examples/flow-log/main.tf",
        "../../modules/flow-log",
        &["examples/flow-log", "modules/flow-log", ""],
    );
    assert!(resolved_to(&got, "modules/flow-log"), "{got:?}");
    assert!(!resolved_to(&got, "examples/flow-log"));
}

#[test]
fn a_local_path_naming_no_scanned_directory_is_a_miss_and_not_a_package() {
    // `External` sits outside both terms of the rate, so a local path is
    // never allowed to land there: it names this repository by construction.
    let got = only("examples/simple/main.tf", "../../modules/absent", &[""]);
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
    // A directory that exists and holds no `.tf` file declares no container,
    // and the answer is the same: the lookup was complete and named nothing.
    let got = only("main.tf", "./docs", &["", "modules/flow-log"]);
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
}

#[test]
fn a_path_that_climbs_past_the_repository_root_is_a_miss() {
    let got = only("main.tf", "../../elsewhere", &[""]);
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
}

#[test]
fn a_dot_segment_is_not_a_directory() {
    let got = only(
        "examples/simple/main.tf",
        ".././simple",
        &["examples/simple"],
    );
    assert!(resolved_to(&got, "examples/simple"), "{got:?}");
}

// ---------------------------------------------------------------------------
// A remote package is outside this repository, by Terraform's own grammar
// ---------------------------------------------------------------------------

#[test]
fn a_registry_address_is_external() {
    // The corpus writes exactly one, at examples/flow-log/main.tf:102.
    let got = only(
        "examples/flow-log/main.tf",
        "terraform-aws-modules/s3-bucket/aws",
        &[""],
    );
    assert_eq!(
        got,
        Outcome::External("terraform-aws-modules/s3-bucket/aws".to_string()),
        "{got:?}",
    );
}

#[test]
fn every_remote_form_terraform_documents_is_external() {
    for (spec, package) in [
        // Registry, with and without a host.
        (
            "terraform-aws-modules/vpc/aws",
            "terraform-aws-modules/vpc/aws",
        ),
        (
            "app.terraform.io/example-corp/vpc/aws",
            "app.terraform.io/example-corp/vpc/aws",
        ),
        // GitHub and Bitbucket shorthand.
        (
            "github.com/hashicorp/example",
            "github.com/hashicorp/example",
        ),
        // A forced source type.
        (
            "git::https://example.com/vpc.git",
            "git::https://example.com/vpc.git",
        ),
        (
            "s3::https://s3-eu-west-1.amazonaws.com/bucket/vpc.zip",
            "s3::https://s3-eu-west-1.amazonaws.com/bucket/vpc.zip",
        ),
        // A URL.
        (
            "https://example.com/vpc-module.zip",
            "https://example.com/vpc-module.zip",
        ),
        (
            "git@github.com:hashicorp/example.git",
            "git@github.com:hashicorp/example.git",
        ),
        // A sub-directory of a package: the package is what is external.
        (
            "terraform-aws-modules/vpc/aws//modules/x",
            "terraform-aws-modules/vpc/aws",
        ),
    ] {
        let got = only("main.tf", spec, &[""]);
        assert_eq!(got, Outcome::External(package.to_string()), "{spec}");
    }
}

#[test]
fn an_address_that_is_no_package_and_no_path_counts_against_the_rate() {
    // The laundering guard. Terraform reads a source from disk only when it
    // begins with `./` or `../`, so `modules/flow-log` is *not* the directory
    // beside this file — but it is also no package address Terraform can
    // fetch. Calling it `External` would move a reference this repository
    // wrote outside both terms of the rate; the answer that costs the rate is
    // the only one that cannot launder a miss.
    let got = only("main.tf", "modules/flow-log", &["", "modules/flow-log"]);
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
    for spec in ["vpc", "a/b/", "a//b"] {
        let got = only("main.tf", spec, &[""]);
        assert_eq!(reason(&got), Some(&ModuleNotFound), "{spec}: {got:?}");
    }
}

#[test]
fn a_package_address_shadowing_a_real_directory_counts_against_the_rate() {
    // The shape the grammar alone cannot settle. `modules/network/vpc` is a
    // valid registry address — `<namespace>/<name>/<provider>`, three bare
    // words — and an ordinary in-repository layout spelled identically. When
    // the directory is there the text is one dropped `./` from a reference
    // this repository wrote, and `External` would carry it out of both terms
    // of the rate; the answer that costs the rate is the only one that
    // cannot launder a miss.
    let got = only(
        "main.tf",
        "modules/network/vpc",
        &["", "modules/network/vpc"],
    );
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
    // Relative to the file that wrote it, exactly as a local path is — a
    // directory of that name elsewhere in the tree shadows nothing.
    let got = only(
        "examples/complete/main.tf",
        "modules/network/vpc",
        &[
            "",
            "examples/complete",
            "examples/complete/modules/network/vpc",
        ],
    );
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
    let got = only(
        "examples/complete/main.tf",
        "modules/network/vpc",
        &["", "modules/network/vpc"],
    );
    assert_eq!(
        got,
        Outcome::External("modules/network/vpc".to_string()),
        "a directory the caller cannot reach shadows nothing: {got:?}",
    );
}

#[test]
fn a_genuine_registry_address_stays_external_beside_a_full_directory_table() {
    // The guard must not cost the corpus its one honest `External` row: the
    // shadow is a directory that is really there, not any name that rhymes.
    let got = only(
        "examples/flow-log/main.tf",
        "terraform-aws-modules/s3-bucket/aws",
        &[
            "",
            "examples/flow-log",
            "modules/flow-log",
            "modules/vpc-endpoints",
        ],
    );
    assert_eq!(
        got,
        Outcome::External("terraform-aws-modules/s3-bucket/aws".to_string()),
        "{got:?}",
    );
}

#[test]
fn four_bare_words_are_no_address_and_never_external() {
    // Terraform's four-segment form is `<host>/<namespace>/<name>/<provider>`
    // and a host holds a dot or a port. Without that test the `External`
    // bucket — the one that sits outside both terms of the rate — widens for
    // any four-segment string at all.
    for spec in [
        "a/b/c/d",
        "modules/team/network/vpc",
        "localhost/corp/vpc/aws",
    ] {
        let got = only("main.tf", spec, &[""]);
        assert_eq!(reason(&got), Some(&ModuleNotFound), "{spec}: {got:?}");
    }
    let got = only("main.tf", "app.terraform.io/example-corp/vpc/aws", &[""]);
    assert_eq!(
        got,
        Outcome::External("app.terraform.io/example-corp/vpc/aws".to_string()),
        "a real host in front is still a real address: {got:?}",
    );
}

#[test]
fn an_empty_literal_names_no_module() {
    let got = only("main.tf", "", &[""]);
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
}

// ---------------------------------------------------------------------------
// A source this build cannot read is a site, and says so
// ---------------------------------------------------------------------------

#[test]
fn a_source_that_is_not_a_literal_is_never_guessed() {
    let got = outcomes(
        "main.tf",
        "module \"a\" {\n  source = var.where\n}\n\nmodule \"b\" {\n  source = \"${path.module}/x\"\n}\n",
        &["", "x"],
    );
    assert_eq!(got.len(), 2);
    for o in &got {
        assert_eq!(reason(o), Some(&DynamicModuleSpecifier), "{o:?}");
    }
}

// ---------------------------------------------------------------------------
// The FQN grammar
// ---------------------------------------------------------------------------

#[test]
fn a_container_and_a_definition_cannot_spell_each_other() {
    // Every HCL identity begins with the `//` a comment opens with, so none
    // can be written by a Terraform identifier and none can reach the
    // `external:` prefix the core reserves.
    assert_eq!(module_fqn(""), "//");
    assert_eq!(module_fqn("examples/simple"), "//examples/simple");
    for fqn in [module_fqn(""), module_fqn("modules/flow-log")] {
        assert!(fqn.starts_with("//"), "{fqn}");
        assert!(!fqn.contains(':'), "{fqn}");
        assert!(!fqn.contains('#'), "a container FQN carries no crossing");
    }
}
