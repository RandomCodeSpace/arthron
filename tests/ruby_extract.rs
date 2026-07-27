//! Extractor fixtures for Ruby, one construct at a time.
//!
//! Ruby is a **tier-2** track: definitions, structure, and imports. The
//! reference kinds asserted here are therefore only [`RefKind::Import`] —
//! a track that emitted calls or type uses un-gated would report tier-1
//! coverage it has not measured.

use arthron::model::{DeclSpace, DefFacets, DefKind, RefKind, TargetRoot};
use arthron::track_ruby::extract::{ImportForm, extract};

/// Every definition as `(kind, owner joined, name)`, in emission order.
fn defs(source: &str) -> Vec<(DefKind, String, String)> {
    extract("lib/app.rb", source)
        .defs
        .into_iter()
        .map(|d| (d.kind, d.owner.join("::"), d.name))
        .collect()
}

/// Every import clause as `(form, raw_target)`.
fn imports(rel: &str, source: &str) -> Vec<(ImportForm, String)> {
    let facts = extract(rel, source);
    let forms: Vec<ImportForm> = facts
        .header
        .imports
        .iter()
        .map(|i| i.form.clone())
        .collect();
    let raws: Vec<String> = facts
        .refs
        .iter()
        .filter(|r| r.kind == RefKind::Import)
        .map(|r| r.raw_target.clone())
        .collect();
    assert_eq!(
        forms.len(),
        raws.len(),
        "an import clause with no reference is a dropped import",
    );
    forms.into_iter().zip(raws).collect()
}

// ---------------------------------------------------------------------------
// The file's own node
// ---------------------------------------------------------------------------

#[test]
fn every_file_declares_the_feature_a_require_names() {
    // Ruby's `require` names a *feature* — the entry `$LOADED_FEATURES` gets
    // — and every `.rb` file is one whether or not it declares a constant.
    // It is emitted first because the driver reads the first `Module`
    // definition as the file's container.
    let facts = extract("lib/rack/utils.rb", "");
    let first = facts.defs.first().expect("a file declares its feature");
    assert_eq!(first.kind, DefKind::Module);
    assert_eq!(first.name, "utils");
    assert!(first.facets.contains(DefFacets::SYNTHETIC));
}

// ---------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------

#[test]
fn a_module_is_a_definition_and_nesting_scopes_the_name() {
    let got = defs("module Rack\n  module Auth\n  end\nend\n");
    assert_eq!(
        got[1..],
        [
            (DefKind::Module, String::new(), "Rack".to_string()),
            (DefKind::Module, "Rack".to_string(), "Auth".to_string()),
        ],
    );
}

#[test]
fn a_class_is_a_type_and_carries_its_enclosing_constants() {
    let got = defs("module Rack\n  class Request\n  end\nend\n");
    assert_eq!(
        got[2],
        (DefKind::Type, "Rack".to_string(), "Request".to_string()),
    );
}

#[test]
fn a_compact_class_name_keeps_the_scope_it_was_written_with() {
    let got = defs("class Rack::Request\nend\n");
    assert_eq!(
        got[1],
        (DefKind::Type, String::new(), "Rack::Request".to_string()),
    );
}

#[test]
fn a_method_is_a_method_when_it_has_an_owner_and_a_function_when_it_does_not() {
    let got = defs("def top; end\nclass C\n  def m; end\nend\n");
    assert_eq!(
        got[1],
        (DefKind::Function, String::new(), "top".to_string()),
    );
    assert_eq!(got[3], (DefKind::Method, "C".to_string(), "m".to_string()));
}

#[test]
fn a_singleton_method_is_static_and_a_class_shovel_block_makes_one_too() {
    let facts = extract(
        "lib/app.rb",
        "class C\n  def self.make; end\n  class << self\n    def other; end\n  end\nend\n",
    );
    let statics: Vec<&str> = facts
        .defs
        .iter()
        .filter(|d| d.facets.contains(DefFacets::STATIC))
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(statics, ["make", "other"]);
}

#[test]
fn a_singleton_method_on_a_local_is_not_nameable_and_is_not_a_definition() {
    let got = defs("obj = Object.new\ndef obj.thing; end\n");
    assert_eq!(got.len(), 1, "only the feature node: {got:?}");
}

#[test]
fn a_constant_assignment_is_a_definition_and_a_local_assignment_is_not() {
    let got = defs("module Rack\n  VERSION = \"3\"\n  RETRIES ||= 2\nend\nx = 1\n");
    assert_eq!(
        got[2..],
        [
            (DefKind::Const, "Rack".to_string(), "VERSION".to_string()),
            (DefKind::Const, "Rack".to_string(), "RETRIES".to_string()),
        ],
    );
}

#[test]
fn a_scoped_constant_assignment_puts_the_scope_in_the_owner() {
    let got = defs("Rack::Thing = 1\n");
    assert_eq!(
        got[1],
        (DefKind::Const, "Rack".to_string(), "Thing".to_string()),
    );
}

#[test]
fn attribute_declarations_are_properties() {
    let got =
        defs("class C\n  attr_reader :env, :body\n  attr_writer :out\n  attr_accessor :x\nend\n");
    let names: Vec<&str> = got
        .iter()
        .filter(|(k, _, _)| *k == DefKind::Property)
        .map(|(_, _, n)| n.as_str())
        .collect();
    assert_eq!(names, ["env", "body", "out", "x"]);
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

#[test]
fn require_relative_and_require_are_different_forms() {
    let got = imports(
        "lib/rack/request.rb",
        "require 'time'\nrequire_relative 'utils'\n",
    );
    assert_eq!(
        got,
        [
            (
                ImportForm::LoadPath("time".to_string()),
                "require 'time'".to_string()
            ),
            (
                ImportForm::Relative("utils".to_string()),
                "require_relative 'utils'".to_string(),
            ),
        ],
    );
}

#[test]
fn an_autoload_names_the_file_its_second_argument_spells() {
    let got = imports(
        "lib/rack.rb",
        "module Rack\n  autoload :Builder, \"rack/builder\"\nend\n",
    );
    assert_eq!(
        got,
        [(
            ImportForm::LoadPath("rack/builder".to_string()),
            "autoload :Builder, \"rack/builder\"".to_string(),
        )],
    );
}

#[test]
fn two_autoloads_of_one_file_are_two_sites_and_not_one_row() {
    // `raw_target` is the literal text at the site and a row is keyed on it,
    // so dropping the constant would key both of these on `autoload
    // "rack/recursive"` — one row, at the first line, with the second
    // declaration nowhere in the output. rack writes exactly this pair.
    let got = imports(
        "lib/rack.rb",
        "module Rack\n  autoload :ForwardRequest, \"rack/recursive\"\n  \
         autoload :Recursive, \"rack/recursive\"\nend\n",
    );
    let raws: Vec<&str> = got.iter().map(|(_, raw)| raw.as_str()).collect();
    assert_eq!(
        raws,
        [
            "autoload :ForwardRequest, \"rack/recursive\"",
            "autoload :Recursive, \"rack/recursive\"",
        ],
    );
}

#[test]
fn an_interpolated_specifier_is_dynamic_and_is_never_guessed() {
    let got = imports("lib/app.rb", "require \"rack/#{name}\"\nrequire path\n");
    assert_eq!(
        got,
        [
            (ImportForm::Dynamic, "require \"rack/#{name}\"".to_string()),
            (ImportForm::Dynamic, "require path".to_string()),
        ],
    );
}

#[test]
fn a_chained_string_literal_is_still_a_literal() {
    let got = imports("lib/app.rb", "require \"rack/\" \"utils\"\n");
    assert_eq!(got[0].0, ImportForm::LoadPath("rack/utils".to_string()));
}

#[test]
fn an_import_reference_names_a_module_and_binds_nothing_locally() {
    let facts = extract("lib/app.rb", "require_relative 'utils'\n");
    let r = &facts.refs[0];
    assert_eq!(r.kind, RefKind::Import);
    assert_eq!(r.space, DeclSpace::Namespace);
    assert_eq!(r.target.root, TargetRoot::Name);
    assert_eq!(r.target.segments, ["utils"]);
    assert!(!r.locally_bound);
    assert_eq!(r.argc, None);
    assert_eq!(r.enclosing, None);
}

#[test]
fn a_dynamic_specifier_has_no_name_at_its_root() {
    let facts = extract("lib/app.rb", "require path\n");
    assert_eq!(facts.refs[0].target.root, TargetRoot::Expr);
}

#[test]
fn an_autoload_inside_a_module_sources_at_that_module() {
    let facts = extract(
        "lib/rack.rb",
        "module Rack\n  autoload :B, \"rack/b\"\nend\n",
    );
    let enc = facts.refs[0].enclosing.as_ref().expect("an encloser");
    assert_eq!(enc.path, ["Rack"]);
    assert_eq!(enc.kind, DefKind::Module);
}

#[test]
fn kernel_require_is_the_same_import_spelled_out() {
    // `require` is `Kernel`'s own method, so `Kernel.require 'time'` is the
    // receiverless site written in full. Dropping it would be an import the
    // resolver never sees — and dropping a require can only *raise* the rate,
    // which is the one direction a shortfall must never take.
    let got = imports("lib/app.rb", "Kernel.require 'time'\n");
    assert_eq!(
        got,
        [(
            ImportForm::LoadPath("time".to_string()),
            "Kernel.require 'time'".to_string(),
        )],
    );
}

#[test]
fn any_other_receiver_is_not_this_extractors_import() {
    // A recorded under-count, asserted so it cannot change silently: a
    // receiver that is not `Kernel` names a runtime value or another module's
    // own `autoload`, and neither is guessed at.
    for source in ["obj.require 'time'\n", "Foo.autoload :Bar, 'foo/bar'\n"] {
        let facts = extract("lib/app.rb", source);
        assert!(facts.refs.is_empty(), "{source}: {:?}", facts.refs);
        assert!(facts.header.imports.is_empty(), "{source}");
    }
}

#[test]
fn a_receiver_does_not_turn_a_call_into_a_declaration() {
    // `attr_reader` declares on the enclosing module; `C.attr_reader` is a
    // call on something else, and the receiver allowlist that lets
    // `Kernel.require` through must not let this through with it.
    let facts = extract("lib/app.rb", "class C\n  Kernel.attr_reader :env\nend\n");
    let names: Vec<&str> = facts.defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["app", "C"], "{names:?}");
}

#[test]
fn no_call_or_type_reference_is_emitted_at_tier_two() {
    // The tier-2 contract, asserted rather than assumed: emitting calls or
    // type uses un-gated would fake tier-1 coverage.
    let facts = extract(
        "lib/app.rb",
        "require 'time'\nclass C < Base\n  include Helpers\n  def m\n    helper(1)\n  end\nend\n",
    );
    // Length first: without it this passes on an empty list, which is what a
    // test naming the tier-2 contract must never do.
    assert_eq!(facts.refs.len(), 1, "{:?}", facts.refs);
    for r in &facts.refs {
        assert_eq!(r.kind, RefKind::Import, "{:?}", r.raw_target);
    }
}
