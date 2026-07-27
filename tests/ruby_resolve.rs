//! Resolver fixtures for Ruby: one import model, one outcome per reference.
//!
//! The import model these pin is the one measured on the corpus:
//! `require_relative` resolves against the requiring file, `require` and
//! `autoload` against the load path, and a specifier that is not a literal
//! resolves against nothing and says so.

use std::collections::HashSet;

use arthron::UnresolvedReason::{self, *};
use arthron::lang::Resolver;
use arthron::model::{Domain, NodeId, node_id};
use arthron::track_ruby::extract::extract;
use arthron::track_ruby::lang::feature_fqn;
use arthron::track_ruby::project::RubyProject;
use arthron::track_ruby::resolve::RubyResolver;
use arthron::{Outcome, resolution_rate};

/// A project with the given load roots and declared gems.
fn project(roots: &[&str], gems: &[&str]) -> RubyProject {
    RubyProject {
        load_roots: roots.iter().map(|r| (*r).to_string()).collect(),
        dependencies: gems.iter().map(|g| (*g).to_string()).collect(),
        gemspecs: Vec::new(),
    }
}

/// A symbol table holding one feature node per repo-relative `.rb` path.
fn table(files: &[&str]) -> HashSet<NodeId> {
    files
        .iter()
        .map(|f| node_id(Domain::Ruby, &feature_fqn(f)))
        .collect()
}

/// Every reference in one file, resolved.
fn outcomes(
    cfg: &RubyProject,
    rel: &str,
    source: &str,
    known: &[&str],
) -> Vec<Outcome<NodeId, String>> {
    let known = table(known);
    let facts = extract(rel, source);
    let scope = RubyResolver.scope(cfg, &facts, &known);
    facts
        .refs
        .iter()
        .map(|r| RubyResolver.resolve(cfg, &scope, r, &known).outcome)
        .collect()
}

/// The single outcome of a one-reference file.
fn only(cfg: &RubyProject, rel: &str, source: &str, known: &[&str]) -> Outcome<NodeId, String> {
    let mut got = outcomes(cfg, rel, source, known);
    assert_eq!(got.len(), 1, "{got:?}");
    got.remove(0)
}

fn reason(o: &Outcome<NodeId, String>) -> Option<&UnresolvedReason> {
    o.unresolved_reason()
}

fn resolved_to(o: &Outcome<NodeId, String>, file: &str) -> bool {
    *o == Outcome::Resolved(node_id(Domain::Ruby, &feature_fqn(file)))
}

// ---------------------------------------------------------------------------
// require_relative: against the requiring file
// ---------------------------------------------------------------------------

#[test]
fn require_relative_resolves_against_the_requiring_files_directory() {
    let cfg = project(&["lib"], &[]);
    let got = only(
        &cfg,
        "lib/rack/request.rb",
        "require_relative 'utils'\n",
        &["lib/rack/utils.rb"],
    );
    assert!(resolved_to(&got, "lib/rack/utils.rb"), "{got:?}");
}

#[test]
fn require_relative_walks_up_out_of_its_own_directory() {
    let cfg = project(&["lib"], &[]);
    let got = only(
        &cfg,
        "test/spec_lint.rb",
        "require_relative '../lib/rack/lint'\n",
        &["lib/rack/lint.rb"],
    );
    assert!(resolved_to(&got, "lib/rack/lint.rb"), "{got:?}");
}

#[test]
fn a_sibling_helper_is_reached_from_outside_any_load_root() {
    let cfg = project(&["lib"], &[]);
    let got = only(
        &cfg,
        "test/spec_request.rb",
        "require_relative 'helper'\n",
        &["test/helper.rb"],
    );
    assert!(resolved_to(&got, "test/helper.rb"), "{got:?}");
}

#[test]
fn a_require_relative_naming_no_file_is_module_not_found() {
    let cfg = project(&["lib"], &[]);
    let got = only(&cfg, "lib/rack/a.rb", "require_relative 'gone'\n", &[]);
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
}

#[test]
fn a_require_relative_escaping_the_repository_is_not_guessed() {
    let cfg = project(&["lib"], &[]);
    let got = only(
        &cfg,
        "lib/a.rb",
        "require_relative '../../elsewhere'\n",
        &[],
    );
    assert_eq!(reason(&got), Some(&ModuleNotFound), "{got:?}");
}

#[test]
fn an_explicit_rb_suffix_names_the_same_feature() {
    let cfg = project(&["lib"], &[]);
    let got = only(
        &cfg,
        "lib/rack/a.rb",
        "require_relative 'utils.rb'\n",
        &["lib/rack/utils.rb"],
    );
    assert!(resolved_to(&got, "lib/rack/utils.rb"), "{got:?}");
}

// ---------------------------------------------------------------------------
// require and autoload: against the load path
// ---------------------------------------------------------------------------

#[test]
fn require_resolves_against_each_load_root_in_order() {
    let cfg = project(&["lib"], &[]);
    let got = only(
        &cfg,
        "lib/rack.rb",
        "require 'rack/utils'\n",
        &["lib/rack/utils.rb"],
    );
    assert!(resolved_to(&got, "lib/rack/utils.rb"), "{got:?}");
}

#[test]
fn an_autoload_resolves_the_file_its_string_names() {
    let cfg = project(&["lib"], &[]);
    let got = only(
        &cfg,
        "lib/rack.rb",
        "module Rack\n  autoload :Builder, \"rack/builder\"\nend\n",
        &["lib/rack/builder.rb"],
    );
    assert!(resolved_to(&got, "lib/rack/builder.rb"), "{got:?}");
}

#[test]
fn a_declared_gem_is_external_and_sits_outside_both_rate_terms() {
    let cfg = project(&["lib"], &["minitest"]);
    let got = only(
        &cfg,
        "test/helper.rb",
        "require 'minitest/global_expectations/autorun'\n",
        &[],
    );
    assert_eq!(got, Outcome::External("minitest".to_string()), "{got:?}");
}

#[test]
fn a_require_that_is_neither_in_repo_nor_declared_is_an_unknown_package() {
    // Ruby's standard library is not indexed here, so `require 'time'` names
    // something real and outside the repository that this build cannot
    // account for. It counts against the rate rather than being waved
    // through as external — widening `External` is how a rate rises with
    // nothing linked.
    let cfg = project(&["lib"], &["minitest"]);
    let got = only(&cfg, "lib/rack/utils.rb", "require 'time'\n", &[]);
    assert_eq!(reason(&got), Some(&UnknownPackage), "{got:?}");
}

#[test]
fn an_in_repo_file_wins_over_a_declared_gem_of_the_same_name() {
    let cfg = project(&["lib"], &["rack"]);
    let got = only(&cfg, "test/helper.rb", "require 'rack'\n", &["lib/rack.rb"]);
    assert!(resolved_to(&got, "lib/rack.rb"), "{got:?}");
}

#[test]
fn a_cwd_anchored_require_is_resolved_against_the_repository_root() {
    let cfg = project(&["lib"], &[]);
    let got = only(
        &cfg,
        "lib/rack/builder.rb",
        "require './app'\n",
        &["app.rb"],
    );
    assert!(resolved_to(&got, "app.rb"), "{got:?}");
}

// ---------------------------------------------------------------------------
// Never guessed, never dropped
// ---------------------------------------------------------------------------

#[test]
fn an_interpolated_specifier_is_unresolved_with_a_dynamic_reason() {
    let cfg = project(&["lib"], &[]);
    for source in ["require \"rack/#{n}\"\n", "require path\n"] {
        let got = only(&cfg, "lib/rack/builder.rb", source, &["lib/rack/x.rb"]);
        assert_eq!(reason(&got), Some(&DynamicModuleSpecifier), "{source}");
    }
}

#[test]
fn every_probe_is_recorded_hit_or_miss() {
    let cfg = project(&["lib", "test", ""], &[]);
    let known = table(&[]);
    let facts = extract("lib/a.rb", "require 'rack/utils'\n");
    let scope = RubyResolver.scope(&cfg, &facts, &known);
    let res = RubyResolver.resolve(&cfg, &scope, &facts.refs[0], &known);
    let want: Vec<NodeId> = ["lib/rack/utils.rb", "test/rack/utils.rb", "rack/utils.rb"]
        .iter()
        .map(|f| node_id(Domain::Ruby, &feature_fqn(f)))
        .collect();
    assert_eq!(res.candidates, want);
}

#[test]
fn local_binding_never_appears_at_tier_two() {
    // No expression-level reference is emitted, so no reference can name a
    // local. The bucket that sits outside both rate terms stays empty, which
    // is what makes this track's rate un-gameable by reclassification.
    let cfg = project(&["lib"], &["minitest"]);
    let got = outcomes(
        &cfg,
        "test/helper.rb",
        "require 'minitest'\nrequire_relative '../lib/rack'\nrequire path\nrequire 'time'\n",
        &["lib/rack.rb"],
    );
    assert!(
        got.iter().all(|o| reason(o) != Some(&LocalBinding)),
        "{got:?}",
    );
    let resolved = got.iter().filter(|o| o.is_resolved()).count() as u64;
    let unresolved = got
        .iter()
        .filter(|o| matches!(o, Outcome::Unresolved(r) if *r != LocalBinding))
        .count() as u64;
    assert_eq!(resolved, 1);
    assert_eq!(unresolved, 2);
    assert_eq!(resolution_rate(resolved, unresolved), Some(1.0 / 3.0));
}

#[test]
fn every_reference_gets_exactly_one_outcome() {
    let cfg = project(&["lib"], &[]);
    let source = concat!(
        "require 'time'\n",
        "require_relative 'utils'\n",
        "require path\n",
        "module Rack\n  autoload :B, 'rack/b'\nend\n",
    );
    let facts = extract("lib/rack.rb", source);
    let got = outcomes(&cfg, "lib/rack.rb", source, &["lib/rack/utils.rb"]);
    assert_eq!(got.len(), facts.refs.len());
    assert_eq!(got.len(), 4);
}
