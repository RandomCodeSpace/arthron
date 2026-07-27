//! Fixtures for the HCL extractor: what a `.tf` file declares, and the one
//! import-like reference HCL has.
//!
//! HCL is **tier 2, best effort**: definitions, structure, and import-like
//! references. The language has no import statement at all — the only site
//! that names something declared elsewhere is a `module` block's `source`
//! attribute, and what it names is a *directory*. Everything else a
//! Terraform file writes — `var.x`, `local.y`, `module.m.out`,
//! `aws_vpc.this.id` — is expression-level and deliberately not emitted: a
//! tier-2 track that emitted them would put references into a denominator
//! nothing here resolves.

use arthron::model::{DeclSpace, DefKind, RefKind};
use arthron::track_hcl::extract::{SourceForm, extract};

/// Every definition as `(kind, owner-joined, name, line)`, in source order.
fn defs(rel: &str, src: &str) -> Vec<(DefKind, String, String, u32)> {
    extract(rel, src)
        .defs
        .iter()
        .map(|d| (d.kind, d.owner.join("."), d.name.clone(), d.span.line))
        .collect()
}

#[test]
fn every_file_declares_the_directory_its_definitions_live_in() {
    // The container is first, so the driver reads it as the file's own — and
    // it is the *directory*, because in HCL two files in one directory are
    // one namespace by position in the filesystem alone.
    let found = defs(
        "examples/simple/main.tf",
        "resource \"aws_vpc\" \"this\" {}\n",
    );
    assert_eq!(
        found[0],
        (DefKind::Module, String::new(), "simple".to_string(), 1),
    );
    let facts = extract("examples/simple/main.tf", "");
    assert_eq!(facts.defs.len(), 1, "an empty file is a file, not an error");
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].space, DeclSpace::Namespace);
    assert!(facts.refs.is_empty());
    // A file at the top of the repository still sits in a directory.
    let root = extract("main.tf", "");
    assert_eq!(root.defs[0].name, "");
}

#[test]
fn each_block_type_declares_its_own_address() {
    let src = "\
resource \"aws_vpc\" \"this\" {
  cidr_block = var.cidr
}

data \"aws_region\" \"current\" {}

variable \"vpc_cidr\" {
  type = string
}

output \"vpc_id\" {
  value = aws_vpc.this.id
}

module \"vpc\" {
  source = \"./child\"
}

locals {
  name = \"ex\"
  tags = { A = 1 }
}
";
    assert_eq!(
        defs("main.tf", src),
        [
            (DefKind::Module, String::new(), String::new(), 1),
            (
                DefKind::Var,
                "resource.aws_vpc".to_string(),
                "this".to_string(),
                1
            ),
            (
                DefKind::Var,
                "data.aws_region".to_string(),
                "current".to_string(),
                5
            ),
            (DefKind::Var, "var".to_string(), "vpc_cidr".to_string(), 7),
            (
                DefKind::Field,
                "output".to_string(),
                "vpc_id".to_string(),
                11
            ),
            (DefKind::Var, "module".to_string(), "vpc".to_string(), 15),
            // One definition per attribute of a `locals` block: the block has
            // no label and declares nothing itself, and `local.name` is what
            // a reference elsewhere spells.
            (DefKind::Const, "local".to_string(), "name".to_string(), 20),
            (DefKind::Const, "local".to_string(), "tags".to_string(), 21),
        ],
    );
}

#[test]
fn a_nested_block_declares_nothing() {
    // The trap: a `block` node matches at every depth, and Terraform gives
    // block types meaning only at the top level of a file. `provider_meta
    // "aws"` inside `terraform` is not a provider, and a nested block that
    // happens to be spelled `variable` is not an input variable.
    let src = "\
terraform {
  required_providers {
    aws = {
      source = \"hashicorp/aws\"
    }
  }
  provider_meta \"aws\" {
    user_agent = [\"x\"]
  }
}

provider \"aws\" {
  region = \"eu-west-1\"
}

resource \"aws_security_group\" \"this\" {
  dynamic \"ingress\" {
    for_each = var.rules
    content {
      from_port = 1
    }
  }
  variable \"not-a-variable\" {
    x = 1
  }
  locals {
    y = 2
  }
}
";
    assert_eq!(
        defs("main.tf", src),
        [
            (DefKind::Module, String::new(), String::new(), 1),
            (
                DefKind::Var,
                "resource.aws_security_group".to_string(),
                "this".to_string(),
                16
            ),
        ],
        "a nested block is structure, never a declaration",
    );
    // And the `source` inside `required_providers` is a provider constraint,
    // not a module source: no reference at all.
    assert!(extract("main.tf", src).refs.is_empty());
    assert!(extract("main.tf", src).header.sources.is_empty());
}

#[test]
fn a_module_source_is_the_only_reference_hcl_emits() {
    let src = "\
module \"vpc\" {
  source = \"../../\"

  name = local.name
  tags = local.tags
}

output \"id\" {
  value = module.vpc.vpc_id
}

resource \"aws_route\" \"r\" {
  vpc = aws_vpc.this.id
  n   = var.n
}
";
    let facts = extract("examples/simple/main.tf", src);
    assert_eq!(
        facts.refs.len(),
        1,
        "expression-level sites are not references"
    );
    let r = &facts.refs[0];
    assert_eq!(r.kind, RefKind::Import);
    assert_eq!(r.space, DeclSpace::Namespace);
    assert_eq!(r.raw_target, "../../");
    assert_eq!(r.target.segments, ["../../"]);
    assert!(!r.locally_bound);
    assert_eq!(r.argc, None);
    assert_eq!(
        r.span.line, 2,
        "the reference sits at the `source` attribute"
    );
    // The edge starts at the module call, which is a definition this same
    // file declares — so the encloser must spell that definition's address.
    let enclosing = r
        .enclosing
        .as_ref()
        .expect("a module source has an encloser");
    assert_eq!(enclosing.path, ["module", "vpc"]);
    assert_eq!(enclosing.kind, DefKind::Var);
    // Clause and reference are paired by span, which is how the resolver
    // finds the form without the core learning what a `source` is.
    assert_eq!(facts.header.sources.len(), 1);
    assert_eq!(facts.header.sources[0].span, r.span);
    assert_eq!(
        facts.header.sources[0].form,
        SourceForm::Literal("../../".to_string()),
    );
}

#[test]
fn a_source_that_is_not_a_literal_is_never_guessed() {
    let src = "\
module \"interpolated\" {
  source = \"${path.module}/../mod\"
}

module \"computed\" {
  source = var.where
}

module \"heredoc\" {
  source = <<-EOT
    ../x
  EOT
}
";
    let facts = extract("main.tf", src);
    assert_eq!(
        facts.refs.len(),
        3,
        "a site this build cannot read is still a site"
    );
    for spec in &facts.header.sources {
        assert_eq!(spec.form, SourceForm::Dynamic);
    }
    assert_eq!(facts.refs[0].raw_target, "\"${path.module}/../mod\"");
    assert_eq!(facts.refs[1].raw_target, "var.where");
}

#[test]
fn a_module_block_without_a_source_names_nothing() {
    // Invalid Terraform, and it must not become a reference to the empty
    // string. The block is still a declaration.
    let facts = extract("main.tf", "module \"vpc\" {\n  name = 1\n}\n");
    assert!(facts.refs.is_empty());
    assert!(facts.header.sources.is_empty());
    assert_eq!(facts.defs.len(), 2);
    // An empty literal *is* a site: it is a literal that names no module.
    let facts = extract("main.tf", "module \"vpc\" {\n  source = \"\"\n}\n");
    assert_eq!(facts.refs.len(), 1);
    assert_eq!(
        facts.header.sources[0].form,
        SourceForm::Literal(String::new()),
    );
}

#[test]
fn a_commented_out_module_declares_nothing() {
    // The corpus writes one, at examples/ipam/main.tf:64. A line-oriented
    // reader would count it; the tree does not.
    let src = "\
# module \"vpc_ipv6\" {
#   source = \"../..\"
# }

module \"real\" {
  source = \"../..\"
}
";
    let facts = extract("examples/ipam/main.tf", src);
    assert_eq!(facts.refs.len(), 1);
    assert_eq!(facts.defs.len(), 2);
}

#[test]
fn a_label_may_be_written_bare_or_quoted() {
    // HCL's native syntax allows both; Terraform's own style is quoted, and
    // a reader of the graph must not be able to tell which was written.
    let quoted = defs("main.tf", "resource \"aws_vpc\" \"this\" {}\n");
    let bare = defs("main.tf", "resource aws_vpc this {}\n");
    assert_eq!(quoted, bare);
    assert_eq!(quoted[1].1, "resource.aws_vpc");
    assert_eq!(quoted[1].2, "this");
}

#[test]
fn a_block_whose_labels_terraform_would_reject_declares_nothing() {
    // Wrong label count for the block type: no address can be composed, and
    // an invented one would be a node nothing can ever name.
    for src in [
        "resource \"aws_vpc\" {}\n",
        "variable {}\n",
        "variable \"a\" \"b\" {}\n",
        "module {}\n",
        "output {}\n",
        "data \"aws_region\" {}\n",
    ] {
        let found = defs("main.tf", src);
        assert_eq!(found.len(), 1, "{src:?} declared something: {found:?}");
    }
}

#[test]
fn the_tier_two_contract_holds_on_every_fixture() {
    // Structural, and asserted on the fixtures rather than only on the
    // corpus: nothing here may emit a call, a type use or an inheritance
    // reference, and nothing may be marked locally bound.
    let src = "\
locals {
  n = f(var.x)
}

resource \"aws_vpc\" \"this\" {
  tags = merge(local.tags, { A = aws_x.y.z })
}

module \"m\" {
  source = \"./c\"
}
";
    for r in &extract("main.tf", src).refs {
        assert_eq!(r.kind, RefKind::Import);
        assert!(!r.locally_bound);
    }
    assert_eq!(extract("main.tf", src).refs.len(), 1);
}
