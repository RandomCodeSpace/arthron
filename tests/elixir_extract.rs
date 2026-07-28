//! Extractor fixtures for Elixir, one construct at a time.
//!
//! Elixir is a **tier-2, best-effort** track: definitions, structure, and
//! import-like references. The reference kinds asserted here are therefore
//! only [`RefKind::Import`] — the four directives `alias`, `import`,
//! `require` and `use`, each of which names a **module**. A track that
//! emitted calls or type uses un-gated would report tier-1 coverage it has
//! not measured.
//!
//! Two properties get more attention than the rest, because both are traps
//! the earlier tier-2 batches wrote down:
//!
//! - **A nested `defmodule` composes its name from the enclosing one**, and
//!   the composed name appears nowhere in the source. `defmodule Bar` inside
//!   `defmodule Foo` declares `Foo.Bar`; grep for the declaration and you
//!   find nothing.
//! - **A directive's target is a module name, not always an absolute one.**
//!   `alias Plug.Conn` binds `Conn`, and a later `import Conn` names
//!   `Plug.Conn`. Getting that wrong would file an in-repository module as
//!   somebody else's — the external-laundering finding, in Elixir's spelling.

use arthron::model::{DeclSpace, DefFacets, DefKind, RefKind};
use arthron::track_elixir::extract::{Directive, ImportForm, extract};

/// Every definition as `(kind, owner joined, name, arity)`, in emission order.
fn defs(source: &str) -> Vec<(DefKind, String, String, Option<u32>)> {
    extract("lib/app.ex", source)
        .defs
        .into_iter()
        .map(|d| (d.kind, d.owner.join("."), d.name, d.params.map(|p| p.count)))
        .collect()
}

/// Every import clause as `(directive, form, raw_target)`.
fn imports(source: &str) -> Vec<(Directive, ImportForm, String)> {
    let facts = extract("lib/app.ex", source);
    let clauses: Vec<(Directive, ImportForm)> = facts
        .header
        .imports
        .iter()
        .map(|i| (i.directive, i.form.clone()))
        .collect();
    let raws: Vec<String> = facts.refs.iter().map(|r| r.raw_target.clone()).collect();
    assert_eq!(
        clauses.len(),
        raws.len(),
        "a directive with no reference is a dropped import",
    );
    clauses
        .into_iter()
        .zip(raws)
        .map(|((d, f), r)| (d, f, r))
        .collect()
}

/// The module a single-clause fixture names.
fn named(source: &str) -> ImportForm {
    let mut got = imports(source);
    assert_eq!(got.len(), 1, "{got:?}");
    got.remove(0).1
}

/// `ImportForm::Module` spelled as a dotted string, for readable assertions.
fn dotted(form: &ImportForm) -> String {
    match form {
        ImportForm::Module(segments) => segments.join("."),
        ImportForm::Dynamic => "<dynamic>".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Definitions: modules
// ---------------------------------------------------------------------------

#[test]
fn a_module_is_a_definition_named_exactly_as_written() {
    // An Elixir module name is one atom. `Plug.Conn` is not `Conn` inside
    // `Plug`; the dots are characters in a name, not steps through
    // containers, so nothing splits them.
    let got = defs("defmodule Plug.Conn do\nend\n");
    assert_eq!(
        got,
        [(
            DefKind::Module,
            String::new(),
            "Plug.Conn".to_string(),
            None
        )],
    );
}

#[test]
fn a_nested_defmodule_composes_its_name_from_the_enclosing_one() {
    // The finding this fixture exists for: the composed name is written
    // nowhere. `Plug.CSRFProtection.InvalidCSRFTokenError` appears in this
    // source zero times.
    let source =
        "defmodule Plug.CSRFProtection do\n  defmodule InvalidCSRFTokenError do\n  end\nend\n";
    assert!(!source.contains("Plug.CSRFProtection.InvalidCSRFTokenError"));
    let got = defs(source);
    assert_eq!(
        got,
        [
            (
                DefKind::Module,
                String::new(),
                "Plug.CSRFProtection".to_string(),
                None,
            ),
            (
                DefKind::Module,
                "Plug.CSRFProtection".to_string(),
                "InvalidCSRFTokenError".to_string(),
                None,
            ),
        ],
    );
}

#[test]
fn nesting_composes_through_a_dotted_inner_name_too() {
    let got = defs("defmodule A do\n  defmodule B.C do\n  end\nend\n");
    assert_eq!(got[1].1, "A");
    assert_eq!(got[1].2, "B.C");
}

#[test]
fn a_nested_module_head_is_not_read_through_the_alias_environment() {
    // `Kernel.defmodule/2` concatenates the enclosing module with the
    // **literal** head segment; it does not expand the head against the
    // alias environment first. So `alias Bar.Baz` in scope leaves
    // `defmodule Baz` declaring `Foo.Baz`, not `Bar.Baz`.
    //
    // The direction this guards is rate inflation, not a lost node: if the
    // head expanded, this file would mint a definition for `Bar.Baz` — a
    // module the repository never declares — and every `alias Bar.Baz` or
    // `import Bar.Baz` anywhere in the tree would then find an
    // in-repository node to resolve against and be counted `Resolved`
    // instead of `External`.
    let got = defs("defmodule Foo do\n  alias Bar.Baz\n  defmodule Baz do\n  end\nend\n");
    let modules: Vec<(String, String)> = got
        .iter()
        .filter(|d| d.0 == DefKind::Module)
        .map(|d| (d.1.clone(), d.2.clone()))
        .collect();
    assert_eq!(
        modules,
        vec![
            (String::new(), "Foo".to_string()),
            ("Foo".to_string(), "Baz".to_string()),
        ],
        "{got:?}",
    );
}

#[test]
fn a_top_level_module_head_is_read_through_the_alias_environment() {
    // The boundary of the rule above, pinned so the fix stays scoped to the
    // nested case. With no enclosing module there is nothing to concatenate
    // onto, and `defmodule` falls back to the expanded alias — so a
    // top-level `defmodule Baz` under `alias Bar.Baz` really does declare
    // `Bar.Baz`.
    let got = defs("alias Bar.Baz\ndefmodule Baz do\nend\n");
    let modules: Vec<(String, String)> = got
        .iter()
        .filter(|d| d.0 == DefKind::Module)
        .map(|d| (d.1.clone(), d.2.clone()))
        .collect();
    assert_eq!(
        modules,
        vec![(String::new(), "Bar.Baz".to_string())],
        "{got:?}",
    );
}

#[test]
fn a_directive_drops_the_elixir_root_exactly_as_a_declaration_does() {
    // `Elixir.Foo.Bar` and `Foo.Bar` are two spellings of one atom. The
    // declaration path strips the root, so the reference path must too —
    // otherwise an in-repository module written with the explicit root can
    // never meet its own declaration, and misses into `External`, outside
    // both terms of the rate.
    let got = imports("defmodule A do\n  import Elixir.Foo.Bar\nend\n");
    assert_eq!(
        got,
        vec![(
            Directive::Import,
            ImportForm::Module(vec!["Foo".to_string(), "Bar".to_string()]),
            "import Elixir.Foo.Bar".to_string(),
        )],
        "{got:?}",
    );
}

#[test]
fn a_module_named_exactly_elixir_keeps_its_name() {
    // The root is only a root when something follows it.
    let got = imports("defmodule A do\n  alias Elixir\nend\n");
    assert_eq!(
        got,
        vec![(
            Directive::Alias,
            ImportForm::Module(vec!["Elixir".to_string()]),
            "alias Elixir".to_string(),
        )],
        "{got:?}",
    );
}

#[test]
fn an_elixir_prefixed_name_is_absolute() {
    // `Elixir.` is the root every module name already carries, so writing it
    // escapes the enclosing module rather than nesting under it.
    let got = defs("defmodule A do\n  defmodule Elixir.Top do\n  end\nend\n");
    assert_eq!(got[1].1, "", "{got:?}");
    assert_eq!(got[1].2, "Top");
}

#[test]
fn a_module_declares_a_namespace_and_a_function_declares_a_value() {
    let facts = extract("lib/app.ex", "defmodule A do\n  def f, do: 1\nend\n");
    assert_eq!(facts.defs[0].space, DeclSpace::Namespace);
    assert_eq!(facts.defs[1].space, DeclSpace::Value);
}

#[test]
fn a_protocol_is_a_module_and_its_heads_have_no_body() {
    let got = defs("defprotocol Plug.Exception do\n  def status(exception)\nend\n");
    assert_eq!(got[0].0, DefKind::Module);
    assert_eq!(got[0].2, "Plug.Exception");
    assert_eq!(got[1].0, DefKind::Function);
    assert_eq!(got[1].1, "Plug.Exception");
    assert_eq!((got[1].2.as_str(), got[1].3), ("status", Some(1)));
    let facts = extract(
        "lib/app.ex",
        "defprotocol Plug.Exception do\n  def status(exception)\nend\n",
    );
    assert!(facts.defs[1].facets.contains(DefFacets::ABSTRACT));
}

#[test]
fn a_defimpl_composes_the_protocol_and_the_type_it_is_for() {
    // `defimpl P, for: T` defines the module `P.T` — the language's own
    // rule, and the name is written nowhere.
    let got = defs("defimpl Plug.Exception, for: Any do\n  def status(_), do: 500\nend\n");
    assert_eq!(got[0].0, DefKind::Module);
    assert_eq!(
        (got[0].1.as_str(), got[0].2.as_str()),
        ("", "Plug.Exception.Any")
    );
    assert_eq!(got[1].1, "Plug.Exception.Any");
}

#[test]
fn a_defimpl_is_absolute_and_does_not_nest_under_its_enclosing_module() {
    // `defimpl` concatenates the protocol and the type; the module it is
    // written inside contributes nothing to that name.
    let got = defs("defmodule T do\n  defimpl Inspect, for: Plug.Conn do\n  end\nend\n");
    assert_eq!(
        (got[1].1.as_str(), got[1].2.as_str()),
        ("", "Inspect.Plug.Conn")
    );
}

// ---------------------------------------------------------------------------
// Definitions: functions and macros
// ---------------------------------------------------------------------------

#[test]
fn a_function_carries_its_module_and_its_arity() {
    let got = defs(
        "defmodule Plug.Conn do\n  def put_resp_header(conn, key, value) do\n    conn\n  end\nend\n",
    );
    assert_eq!(
        got[1],
        (
            DefKind::Function,
            "Plug.Conn".to_string(),
            "put_resp_header".to_string(),
            Some(3),
        ),
    );
}

#[test]
fn a_function_written_without_parentheses_has_arity_zero() {
    let got = defs("defmodule A do\n  def init, do: []\nend\n");
    assert_eq!(got[1].3, Some(0));
}

#[test]
fn a_guarded_clause_is_the_function_it_guards() {
    let got = defs("defmodule A do\n  def f(x) when is_atom(x), do: x\nend\n");
    assert_eq!((got[1].2.as_str(), got[1].3), ("f", Some(1)));
    assert_eq!(got.len(), 2, "the guard is not a definition: {got:?}");
}

#[test]
fn the_six_def_forms_differ_only_in_visibility_and_in_runtime_presence() {
    let facts = extract(
        "lib/app.ex",
        "defmodule A do\n  def a, do: 1\n  defp b, do: 1\n  defmacro c, do: 1\n  \
         defmacrop d, do: 1\n  defguard e(x) when is_atom(x)\n  defguardp f(x) when is_atom(x)\n  \
         defdelegate g(x), to: B\nend\n",
    );
    let got: Vec<(&str, bool, bool)> = facts.defs[1..]
        .iter()
        .map(|d| {
            (
                d.name.as_str(),
                d.facets.contains(DefFacets::EXPORTED),
                d.facets.contains(DefFacets::RUNTIME),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            // `def` and `defdelegate` define a function that exists when the
            // program runs. A macro is expanded at compile time and is gone
            // by then, which is exactly what `RUNTIME` records.
            ("a", true, true),
            ("b", false, true),
            ("c", true, false),
            ("d", false, false),
            ("e", true, false),
            ("f", false, false),
            ("g", true, true),
        ],
    );
    assert!(facts.defs[1..].iter().all(|d| d.kind == DefKind::Function));
}

#[test]
fn a_head_without_a_body_is_abstract() {
    let facts = extract(
        "lib/app.ex",
        "defmodule A do\n  def f(a, b \\\\ 1)\n  def f(a, b), do: a\nend\n",
    );
    assert!(facts.defs[1].facets.contains(DefFacets::ABSTRACT));
    assert!(!facts.defs[2].facets.contains(DefFacets::ABSTRACT));
}

#[test]
fn a_function_whose_name_is_computed_is_not_a_definition() {
    // `def unquote(name)(x)` names something only the expansion knows.
    let got = defs("defmodule A do\n  def unquote(name)(x), do: x\nend\n");
    assert_eq!(got.len(), 1, "{got:?}");
}

#[test]
fn a_conditional_definition_belongs_to_the_module_that_encloses_the_condition() {
    // A `def` inside `if` is an ordinary module function: `if` is a macro
    // whose block does not open a declaration scope.
    let got = defs(
        "defmodule A do\n  if x do\n    defp v, do: 1\n  else\n    defp v, do: 2\n  end\nend\n",
    );
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[1].1, "A");
    assert_eq!(got[2].1, "A");
}

// ---------------------------------------------------------------------------
// Definitions: structs
// ---------------------------------------------------------------------------

#[test]
fn a_struct_declares_one_field_per_key() {
    let facts = extract(
        "lib/app.ex",
        "defmodule Plug.Upload do\n  defstruct [:path, :filename]\nend\n",
    );
    let fields: Vec<(&str, &str)> = facts.defs[1..]
        .iter()
        .map(|d| {
            assert_eq!(d.kind, DefKind::Field);
            assert!(d.facets.contains(DefFacets::SYNTHETIC));
            (d.owner[0].as_str(), d.name.as_str())
        })
        .collect();
    assert_eq!(
        fields,
        [("Plug.Upload", "path"), ("Plug.Upload", "filename")]
    );
}

#[test]
fn an_exception_declares_one_field_per_key_whatever_shape_it_is_written_in() {
    let got = defs("defmodule E do\n  defexception message: \"x\", plug_status: 400\nend\n");
    assert_eq!(
        got[1..].iter().map(|d| d.2.as_str()).collect::<Vec<_>>(),
        ["message", "plug_status"],
    );
    let mixed = defs("defmodule E do\n  defexception [:conn, message: \"x\"]\nend\n");
    assert_eq!(
        mixed[1..].iter().map(|d| d.2.as_str()).collect::<Vec<_>>(),
        ["conn", "message"],
    );
}

#[test]
fn a_struct_whose_keys_are_computed_declares_no_field() {
    let got = defs("defmodule E do\n  defstruct @fields\nend\n");
    assert_eq!(got.len(), 1, "{got:?}");
}

// ---------------------------------------------------------------------------
// What a macro body declares is not this file's
// ---------------------------------------------------------------------------

#[test]
fn a_definition_inside_a_quote_is_not_this_files() {
    // `quote do def call(...) end` declares a function in whatever module
    // `use`s this one. The owner is not derivable from this file, so no node
    // is invented for it.
    let got = defs(
        "defmodule Plug.Builder do\n  defmacro __using__(_) do\n    quote do\n      \
         def call(conn, _), do: conn\n    end\n  end\nend\n",
    );
    assert_eq!(
        got.iter().map(|d| d.2.as_str()).collect::<Vec<_>>(),
        ["Plug.Builder", "__using__"],
    );
}

#[test]
fn a_module_inside_a_quote_is_not_this_files_either() {
    // `quote do defmodule unquote(name) do ... end end` declares a module
    // when the macro expands, under a name the expansion builds. Recording
    // it here would put a module in the graph that no compiled program has.
    let got = defs(
        "defmodule Gen do\n  defmacro __using__(_) do\n    quote do\n      \
         defmodule Inner do\n        def go, do: 1\n      end\n    end\n  end\nend\n",
    );
    assert_eq!(
        got.iter().map(|d| d.2.as_str()).collect::<Vec<_>>(),
        ["Gen", "__using__"],
    );
}

#[test]
fn a_directive_inside_a_quote_still_names_a_module() {
    // The *target* is knowable even though the module it will be injected
    // into is not: `Plug.Conn` is `Plug.Conn` wherever the expansion lands.
    let source = "defmodule Plug.Builder do\n  defmacro __using__(_) do\n    quote do\n      \
                  import Plug.Conn\n    end\n  end\nend\n";
    assert_eq!(dotted(&named(source)), "Plug.Conn");
    let facts = extract("lib/app.ex", source);
    let enclosing = facts.refs[0].enclosing.as_ref().expect("an encloser");
    // The nearest nameable definition *outside* the quote, which is the
    // macro itself — not the `def` the quote will emit.
    assert_eq!(enclosing.path, ["Plug.Builder", "__using__/1"]);
}

// ---------------------------------------------------------------------------
// The four directives
// ---------------------------------------------------------------------------

#[test]
fn each_of_the_four_directives_names_one_module() {
    let got = imports(
        "defmodule A do\n  alias Plug.Conn\n  import Plug.Test\n  require Logger\n  use Plug.Builder\nend\n",
    );
    assert_eq!(
        got.iter()
            .map(|(d, f, r)| (d.name(), dotted(f), r.as_str()))
            .collect::<Vec<_>>(),
        [
            ("alias", "Plug.Conn".to_string(), "alias Plug.Conn"),
            ("import", "Plug.Test".to_string(), "import Plug.Test"),
            ("require", "Logger".to_string(), "require Logger"),
            ("use", "Plug.Builder".to_string(), "use Plug.Builder"),
        ],
    );
}

#[test]
fn the_options_a_directive_carries_are_not_part_of_what_it_names() {
    for source in [
        "defmodule A do\n  import Plug.Conn, only: [get_req_header: 2]\nend\n",
        "defmodule A do\n  use Plug.Router, init_mode: :runtime\nend\n",
        "defmodule A do\n  alias Plug.Conn, as: C\nend\n",
        "defmodule A do\n  use Plug.Builder,\n    log_on_halt: :debug\nend\n",
    ] {
        let form = named(source);
        assert!(
            matches!(&form, ImportForm::Module(s) if s[0] == "Plug"),
            "{source:?}: {form:?}",
        );
    }
}

#[test]
fn a_multi_alias_names_one_module_per_member() {
    let got = imports("defmodule A do\n  alias Plug.{Conn, Router}\nend\n");
    assert_eq!(
        got.iter()
            .map(|(_, f, r)| (dotted(f), r.as_str()))
            .collect::<Vec<_>>(),
        [
            ("Plug.Conn".to_string(), "alias Plug.Conn"),
            ("Plug.Router".to_string(), "alias Plug.Router"),
        ],
    );
}

#[test]
fn a_directive_whose_target_is_computed_is_never_guessed() {
    for source in [
        "defmodule A do\n  require unquote(target)\nend\n",
        "defmodule A do\n  alias __MODULE__.Sub\nend\n",
        "defmodule A do\n  alias :\"Elixir.Foo\"\nend\n",
    ] {
        assert!(
            matches!(named(source), ImportForm::Dynamic),
            "{source:?} was guessed",
        );
    }
}

#[test]
fn a_directive_with_no_argument_names_nothing_at_all() {
    let facts = extract("lib/app.ex", "defmodule A do\n  use\nend\n");
    assert!(facts.refs.is_empty(), "{:?}", facts.refs);
    assert!(facts.header.imports.is_empty());
}

// ---------------------------------------------------------------------------
// The alias environment: a target is not always an absolute name
// ---------------------------------------------------------------------------

#[test]
fn an_alias_binds_a_name_a_later_directive_may_use() {
    // Elixir's own rule, and the reason it is here: without it `import Conn`
    // names a module this repository does not declare, and gets filed as
    // somebody else's code.
    let got = imports("defmodule A do\n  alias Plug.Conn\n  import Conn\nend\n");
    assert_eq!(dotted(&got[1].1), "Plug.Conn");
}

#[test]
fn an_alias_chain_expands_through_every_binding_before_it() {
    let got =
        imports("defmodule A do\n  alias Plug.Conn\n  alias Conn.Utils\n  import Utils\nend\n");
    assert_eq!(
        got.iter().map(|(_, f, _)| dotted(f)).collect::<Vec<_>>(),
        ["Plug.Conn", "Plug.Conn.Utils", "Plug.Conn.Utils"],
    );
}

#[test]
fn an_as_option_binds_the_name_it_states_and_not_the_last_segment() {
    let got = imports(
        "defmodule A do\n  alias Plug.Session.COOKIE, as: CookieStore\n  import CookieStore\n  import COOKIE\nend\n",
    );
    assert_eq!(dotted(&got[1].1), "Plug.Session.COOKIE");
    // The last segment was never bound, so this one still names what it says.
    assert_eq!(dotted(&got[2].1), "COOKIE");
}

#[test]
fn a_require_with_as_binds_a_name_and_a_require_without_one_does_not() {
    let got = imports("defmodule A do\n  require Plug.Router.Utils, as: R\n  import R\nend\n");
    assert_eq!(dotted(&got[1].1), "Plug.Router.Utils");
    let bare = imports("defmodule A do\n  require Plug.Router.Utils\n  import Utils\nend\n");
    assert_eq!(dotted(&bare[1].1), "Utils");
}

#[test]
fn a_nested_module_binds_its_last_segment_inside_the_module_that_holds_it() {
    let got = imports(
        "defmodule Plug.DebuggerTest do\n  defmodule ActionableError do\n  end\n  \
         alias ActionableError\nend\n",
    );
    assert_eq!(dotted(&got[0].1), "Plug.DebuggerTest.ActionableError");
}

#[test]
fn a_binding_reaches_only_the_module_that_wrote_it() {
    // Two modules in one file is ordinary Elixir — 26 files in the measured
    // corpus do it — and one module's aliases are not the other's.
    let got =
        imports("defmodule A do\n  alias Plug.Conn\nend\n\ndefmodule B do\n  import Conn\nend\n");
    assert_eq!(dotted(&got[1].1), "Conn");
}

#[test]
fn a_binding_written_inside_a_function_body_reaches_nothing_outside_it() {
    // Aliases are block-scoped. Reading one out of a function body and
    // applying it to the whole module would expand a name Elixir would not,
    // which is a wrong answer rather than a missing one.
    let got =
        imports("defmodule A do\n  def f do\n    alias Plug.Conn\n  end\n\n  import Conn\nend\n");
    assert_eq!(dotted(&got[1].1), "Conn");
}

#[test]
fn a_binding_reaches_only_forward() {
    let got = imports("defmodule A do\n  import Conn\n  alias Plug.Conn\nend\n");
    assert_eq!(dotted(&got[0].1), "Conn");
}

// ---------------------------------------------------------------------------
// The tier-2 contract, and the shape of the output
// ---------------------------------------------------------------------------

#[test]
fn no_call_site_and_no_type_use_becomes_a_reference() {
    let facts = extract(
        "lib/app.ex",
        "defmodule A do\n  @behaviour Plug\n  @type t :: Plug.Conn.t()\n  \
         def f(conn), do: Plug.Conn.send_resp(conn, 200, \"\")\nend\n",
    );
    assert!(facts.refs.is_empty(), "{:?}", facts.refs);
}

#[test]
fn every_reference_is_an_import_and_none_is_locally_bound() {
    let facts = extract(
        "lib/app.ex",
        "defmodule A do\n  alias Plug.Conn\n  import Plug.Test\n  use Plug.Builder\nend\n",
    );
    assert!(facts.refs.iter().all(|r| r.kind == RefKind::Import));
    assert!(facts.refs.iter().all(|r| r.space == DeclSpace::Namespace));
    assert!(facts.refs.iter().all(|r| !r.locally_bound));
    assert!(facts.refs.iter().all(|r| r.argc.is_none()));
}

#[test]
fn text_inside_a_doc_string_is_not_a_directive() {
    // The corpus writes `import AnotherModule, only: [...]` inside a
    // `@moduledoc` heredoc. A line-oriented reader counts it; a parser does
    // not, and this is the difference stated as a test.
    let facts = extract(
        "lib/app.ex",
        "defmodule Plug.Builder do\n  @moduledoc \"\"\"\n  Example:\n\n      \
         import AnotherModule, only: [interesting_plug: 2]\n  \"\"\"\nend\n",
    );
    assert!(facts.refs.is_empty(), "{:?}", facts.refs);
}

#[test]
fn records_come_out_in_source_order() {
    let facts = extract(
        "lib/app.ex",
        "defmodule A do\n  alias Plug.Conn\n  def f, do: 1\n  import Plug.Test\n  def g, do: 2\nend\n",
    );
    assert_eq!(
        facts.refs.iter().map(|r| r.span.line).collect::<Vec<_>>(),
        [2, 4],
    );
    assert!(
        facts
            .defs
            .windows(2)
            .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
        "{:?}",
        facts.defs,
    );
    assert!(
        facts
            .header
            .imports
            .windows(2)
            .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
    );
}

#[test]
fn a_reference_at_the_top_of_a_file_has_no_encloser() {
    // `config/config.exs` writes `import Config` outside any module. There
    // is no definition to source an edge at, and Elixir has no file node —
    // no reference in this language names a file.
    let facts = extract("config/config.exs", "import Config\n");
    assert_eq!(facts.refs.len(), 1);
    assert!(facts.refs[0].enclosing.is_none());
    assert!(facts.defs.is_empty());
}

#[test]
fn a_reference_at_module_level_is_enclosed_by_the_module() {
    let facts = extract(
        "lib/app.ex",
        "defmodule Plug.Conn do\n  import Plug.Test\nend\n",
    );
    let enclosing = facts.refs[0].enclosing.as_ref().expect("an encloser");
    assert_eq!(enclosing.path, ["Plug.Conn"]);
    assert_eq!(enclosing.kind, DefKind::Module);
}

#[test]
fn a_reference_inside_a_function_is_enclosed_by_that_function() {
    let facts = extract(
        "lib/app.ex",
        "defmodule A do\n  def f(x) do\n    import Plug.Conn\n    x\n  end\nend\n",
    );
    let enclosing = facts.refs[0].enclosing.as_ref().expect("an encloser");
    assert_eq!(enclosing.path, ["A", "f/1"]);
    assert_eq!(enclosing.kind, DefKind::Function);
}

#[test]
fn a_broken_file_yields_records_rather_than_a_panic() {
    // tree-sitter is error-tolerant, and a file that does not parse is still
    // a file this scan read.
    let facts = extract("lib/broken.ex", "defmodule ((( do\n  alias\n");
    assert!(facts.refs.len() <= 1, "{:?}", facts.refs);
}

#[test]
fn a_file_declaring_nothing_declares_nothing() {
    let facts = extract("lib/empty.ex", "");
    assert!(facts.defs.is_empty());
    assert!(facts.refs.is_empty());
    assert_eq!(facts.header.rel_path, "lib/empty.ex");
}
