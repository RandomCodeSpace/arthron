//! Resolver fixtures for Elixir: one import model, one outcome per reference.
//!
//! The model these pin is the one the corpus measures. **Nothing in Elixir
//! names a file** — there is no import-by-path form in the language — so a
//! directive names a module by the atom it compiles to, and resolution is an
//! exact probe against the modules this repository declares. A module absent
//! from that set is declared somewhere else and is named as such; a target
//! that is not a literal module name is refused rather than guessed.
//!
//! The file that matters most is the last section. `External` sits outside
//! both terms of the resolution rate, so filing an in-repository module into
//! it makes a miss disappear rather than fail. Every way this track could do
//! that has a fixture.

use std::collections::HashSet;

use arthron::UnresolvedReason::{self, DynamicModuleSpecifier};
use arthron::lang::Resolver;
use arthron::model::{Domain, NodeId, node_id};
use arthron::track_elixir::extract::extract;
use arthron::track_elixir::lang::ElixirProject;
use arthron::track_elixir::resolve::ElixirResolver;
use arthron::{Outcome, resolution_rate};

/// A symbol table holding one node per module name.
fn table(modules: &[&str]) -> HashSet<NodeId> {
    modules.iter().map(|m| node_id(Domain::Elixir, m)).collect()
}

/// Every reference in one file, resolved against a set of known modules.
fn outcomes(source: &str, known: &[&str]) -> Vec<Outcome<NodeId, String>> {
    let known = table(known);
    let facts = extract("lib/app.ex", source);
    let scope = ElixirResolver.scope(&ElixirProject, &facts, &known);
    facts
        .refs
        .iter()
        .map(|r| {
            ElixirResolver
                .resolve(&ElixirProject, &scope, r, &known)
                .outcome
        })
        .collect()
}

/// The outcome of the one reference a file writes as `raw_target`.
fn named(source: &str, known: &[&str], raw_target: &str) -> Outcome<NodeId, String> {
    let table = table(known);
    let facts = extract("lib/app.ex", source);
    let scope = ElixirResolver.scope(&ElixirProject, &facts, &table);
    let mut found: Vec<Outcome<NodeId, String>> = facts
        .refs
        .iter()
        .filter(|r| r.raw_target == raw_target)
        .map(|r| {
            ElixirResolver
                .resolve(&ElixirProject, &scope, r, &table)
                .outcome
        })
        .collect();
    assert_eq!(found.len(), 1, "{raw_target}: {found:?}");
    found.remove(0)
}

/// The single outcome of a one-reference file.
fn only(source: &str, known: &[&str]) -> Outcome<NodeId, String> {
    let mut got = outcomes(source, known);
    assert_eq!(got.len(), 1, "{got:?}");
    got.remove(0)
}

fn resolved_to(o: &Outcome<NodeId, String>, module: &str) -> bool {
    *o == Outcome::Resolved(node_id(Domain::Elixir, module))
}

fn reason(o: &Outcome<NodeId, String>) -> Option<&UnresolvedReason> {
    o.unresolved_reason()
}

// ---------------------------------------------------------------------------
// Rule 1: a module this repository declares
// ---------------------------------------------------------------------------

#[test]
fn each_of_the_four_directives_resolves_against_the_module_set() {
    let source = "defmodule A do\n  alias Plug.Conn\n  import Plug.Test\n  require Plug.Router\n  \
                  use Plug.Builder\nend\n";
    let got = outcomes(
        source,
        &["A", "Plug.Conn", "Plug.Test", "Plug.Router", "Plug.Builder"],
    );
    assert!(resolved_to(&got[0], "Plug.Conn"), "{got:?}");
    assert!(resolved_to(&got[1], "Plug.Test"), "{got:?}");
    assert!(resolved_to(&got[2], "Plug.Router"), "{got:?}");
    assert!(resolved_to(&got[3], "Plug.Builder"), "{got:?}");
}

#[test]
fn a_nested_module_is_named_by_the_composed_name_a_reference_writes() {
    // The corpus's own case: `alias Plug.CSRFProtection.InvalidCSRFTokenError`
    // names a module written `defmodule InvalidCSRFTokenError` inside
    // `defmodule Plug.CSRFProtection`. Grep the declaration site for the
    // composed name and you find nothing; the resolver must still link it.
    let declaring =
        "defmodule Plug.CSRFProtection do\n  defmodule InvalidCSRFTokenError do\n  end\nend\n";
    let module = extract("lib/plug/csrf_protection.ex", declaring)
        .defs
        .iter()
        .map(|d| {
            let mut path = d.owner.clone();
            path.push(d.name.clone());
            path.join(".")
        })
        .collect::<Vec<_>>();
    assert!(module.contains(&"Plug.CSRFProtection.InvalidCSRFTokenError".to_string()));
    let got = only(
        "defmodule T do\n  alias Plug.CSRFProtection.InvalidCSRFTokenError\nend\n",
        &[
            "T",
            "Plug.CSRFProtection",
            "Plug.CSRFProtection.InvalidCSRFTokenError",
        ],
    );
    assert!(
        resolved_to(&got, "Plug.CSRFProtection.InvalidCSRFTokenError"),
        "{got:?}",
    );
}

#[test]
fn a_multi_alias_resolves_each_member_on_its_own() {
    let got = outcomes(
        "defmodule A do\n  alias Plug.{Conn, Missing}\nend\n",
        &["A", "Plug.Conn"],
    );
    assert!(resolved_to(&got[0], "Plug.Conn"), "{got:?}");
    assert_eq!(got[1], Outcome::External("Plug.Missing".to_string()));
}

// ---------------------------------------------------------------------------
// Rule 2: a module it does not declare
// ---------------------------------------------------------------------------

#[test]
fn a_module_outside_the_repository_is_named_by_its_whole_module_name() {
    // Not by a root segment. Elixir has no namespace hierarchy: `Plug.Crypto`
    // is one atom and is no more a child of `Plug` than `Plugin` is, so the
    // root would be a guess about an ownership the language does not have.
    let got = only(
        "defmodule A do\n  alias Plug.Crypto.KeyGenerator\nend\n",
        &["A", "Plug", "Plug.Conn"],
    );
    assert_eq!(
        got,
        Outcome::External("Plug.Crypto.KeyGenerator".to_string())
    );
}

#[test]
fn declaring_a_prefix_does_not_claim_what_sits_under_it() {
    // The other half of the same rule, and the reason it is a probe rather
    // than a prefix test: this repository declaring `Plug` says nothing at
    // all about `Plug.Crypto`, which a hex dependency supplies.
    let got = only("defmodule Plug do\n  use ExUnit.Case\nend\n", &["Plug"]);
    assert_eq!(got, Outcome::External("ExUnit.Case".to_string()));
}

#[test]
fn an_external_reference_is_outside_both_terms_of_the_rate() {
    // Stated as arithmetic rather than as prose: three directives, one of
    // which names this repository, and the rate is 1/1 and not 1/3. That is
    // exactly why the bucket is dangerous and why every fixture below exists.
    let got = outcomes(
        "defmodule A do\n  alias Plug.Conn\n  use ExUnit.Case\n  require Logger\nend\n",
        &["A", "Plug.Conn"],
    );
    let resolved = got.iter().filter(|o| o.is_resolved()).count() as u64;
    let external = got
        .iter()
        .filter(|o| matches!(o, Outcome::External(_)))
        .count() as u64;
    let unresolved = got.iter().filter_map(|o| reason(o)).count() as u64;
    assert_eq!((resolved, external, unresolved), (1, 2, 0));
    assert_eq!(resolution_rate(resolved, unresolved), Some(1.0));
}

// ---------------------------------------------------------------------------
// A target that is not a literal module name
// ---------------------------------------------------------------------------

#[test]
fn a_computed_target_resolves_against_nothing_and_says_so() {
    for source in [
        "defmodule A do\n  require unquote(target)\nend\n",
        "defmodule A do\n  alias __MODULE__.Sub\nend\n",
        "defmodule A do\n  alias :\"Elixir.Foo\"\nend\n",
    ] {
        let got = only(source, &["A"]);
        assert_eq!(reason(&got), Some(&DynamicModuleSpecifier), "{source:?}");
    }
}

#[test]
fn a_computed_target_counts_against_the_rate_rather_than_leaving_it() {
    // `DynamicModuleSpecifier` is an `Unresolved` reason, so it sits in the
    // denominator. A build that cannot say which module is named has failed
    // to link something real, and the rate says so.
    let got = outcomes(
        "defmodule A do\n  alias Plug.Conn\n  require unquote(t)\nend\n",
        &["A", "Plug.Conn"],
    );
    let resolved = got.iter().filter(|o| o.is_resolved()).count() as u64;
    let unresolved = got.iter().filter_map(|o| reason(o)).count() as u64;
    assert_eq!(resolution_rate(resolved, unresolved), Some(0.5));
}

// ---------------------------------------------------------------------------
// The laundering guard: an in-repository module must never become External
// ---------------------------------------------------------------------------

#[test]
fn a_head_bound_by_an_alias_resolves_rather_than_leaving_the_measurement() {
    // Without the file's own alias environment, `import Conn` names a module
    // called `Conn`, misses, and is filed as somebody else's code — a real
    // miss removed from both terms of the rate instead of counted in one.
    let got = outcomes(
        "defmodule A do\n  alias Plug.Conn\n  import Conn\nend\n",
        &["A", "Plug.Conn"],
    );
    assert!(resolved_to(&got[1], "Plug.Conn"), "{got:?}");
    assert!(
        !matches!(got[1], Outcome::External(_)),
        "an in-repository module was filed as external",
    );
}

#[test]
fn a_head_bound_by_a_renaming_alias_resolves_too() {
    let got = outcomes(
        "defmodule A do\n  alias Plug.Session.COOKIE, as: CookieStore\n  use CookieStore\nend\n",
        &["A", "Plug.Session.COOKIE"],
    );
    assert!(resolved_to(&got[1], "Plug.Session.COOKIE"), "{got:?}");
}

#[test]
fn a_head_bound_by_a_nested_module_resolves_too() {
    let got = outcomes(
        "defmodule Plug.DebuggerTest do\n  defmodule ActionableError do\n  end\n  \
         alias ActionableError\nend\n",
        &["Plug.DebuggerTest", "Plug.DebuggerTest.ActionableError"],
    );
    assert!(
        resolved_to(&got[0], "Plug.DebuggerTest.ActionableError"),
        "{got:?}",
    );
}

#[test]
fn a_directive_that_writes_the_elixir_root_reaches_the_declaration() {
    // `Elixir.Plug.Conn` and `Plug.Conn` are one atom, and the declaration
    // path files it under the short spelling. A reference path that kept the
    // root would probe a name nothing declares, miss, and file an
    // in-repository module as somebody else's — laundering by spelling.
    for source in [
        "defmodule A do\n  import Elixir.Plug.Conn\nend\n",
        "defmodule A do\n  alias Elixir.Plug.Conn\nend\n",
        "defmodule A do\n  require Elixir.Plug.Conn\nend\n",
        "defmodule A do\n  use Elixir.Plug.Conn\nend\n",
    ] {
        let got = only(source, &["A", "Plug.Conn"]);
        assert!(resolved_to(&got, "Plug.Conn"), "{source:?}: {got:?}");
        assert!(
            !matches!(got, Outcome::External(_)),
            "{source:?}: an in-repository module was filed as external",
        );
    }
}

#[test]
fn the_elixir_root_binds_under_its_last_segment_like_any_other_alias() {
    // The root is stripped before the binding is taken, so `alias
    // Elixir.Plug.Conn` binds `Conn` to `Plug.Conn` exactly as `alias
    // Plug.Conn` does — and a later `import Conn` resolves.
    let got = outcomes(
        "defmodule A do\n  alias Elixir.Plug.Conn\n  import Conn\nend\n",
        &["A", "Plug.Conn"],
    );
    assert!(resolved_to(&got[1], "Plug.Conn"), "{got:?}");
}

#[test]
fn a_name_no_binding_reaches_is_still_named_as_written() {
    // The conservative direction, asserted so it stays conservative: a
    // binding written in another module, or later in the file, or inside a
    // function body, expands nothing — and the reference names what it says
    // and is measured against that, rather than being expanded into a name
    // Elixir would not.
    for source in [
        "defmodule A do\n  alias Plug.Conn\nend\n\ndefmodule B do\n  import Conn\nend\n",
        "defmodule A do\n  import Conn\n  alias Plug.Conn\nend\n",
        "defmodule A do\n  def f do\n    alias Plug.Conn\n  end\n\n  import Conn\nend\n",
    ] {
        let got = named(source, &["A", "B", "Plug.Conn"], "import Conn");
        assert_eq!(got, Outcome::External("Conn".to_string()), "{source:?}");
    }
}

// ---------------------------------------------------------------------------
// Completeness
// ---------------------------------------------------------------------------

#[test]
fn every_reference_ends_in_exactly_one_outcome() {
    let source = "defmodule A do\n  alias Plug.{Conn, Router}\n  import Plug.Test\n  \
                  require unquote(x)\n  use ExUnit.Case\n  alias Plug.Conn, as: C\nend\n";
    let got = outcomes(source, &["A", "Plug.Conn", "Plug.Test"]);
    assert_eq!(got.len(), 6, "{got:?}");
    let resolved = got.iter().filter(|o| o.is_resolved()).count();
    let external = got
        .iter()
        .filter(|o| matches!(o, Outcome::External(_)))
        .count();
    let unresolved = got.iter().filter_map(|o| reason(o)).count();
    assert_eq!(resolved + external + unresolved, got.len());
    assert_eq!((resolved, external, unresolved), (3, 2, 1));
}

#[test]
fn a_probe_is_recorded_whether_it_hit_or_missed() {
    // The candidate list is what wakes a file when a module it named starts
    // existing, so a miss has to record the identity it looked for.
    let known: HashSet<NodeId> = HashSet::new();
    let facts = extract("lib/app.ex", "defmodule A do\n  use ExUnit.Case\nend\n");
    let scope = ElixirResolver.scope(&ElixirProject, &facts, &known);
    let got = ElixirResolver.resolve(&ElixirProject, &scope, &facts.refs[0], &known);
    assert_eq!(got.candidates, vec![node_id(Domain::Elixir, "ExUnit.Case")]);
    // A target nothing can name probes nothing, and must not pretend to.
    let facts = extract("lib/app.ex", "defmodule A do\n  require unquote(x)\nend\n");
    let scope = ElixirResolver.scope(&ElixirProject, &facts, &known);
    let got = ElixirResolver.resolve(&ElixirProject, &scope, &facts.refs[0], &known);
    assert!(got.candidates.is_empty(), "{got:?}");
}
