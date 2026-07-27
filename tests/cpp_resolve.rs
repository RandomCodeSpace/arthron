//! Resolver fixtures for C++: one import model, one outcome per reference.
//!
//! The model these pin is the one the corpus measures. A quoted `#include`
//! starts at the including file's own directory and falls back to the include
//! roots; an angled one starts at the roots and never at the file; a C++20
//! `import` names a module and no path; and a macro specifier names nothing
//! this build may guess at.
//!
//! Two of the cases below are about `External` rather than about linking,
//! because `External` sits outside both terms of the resolution rate and is
//! therefore the one classification a resolver can raise its own rate with.
//! This track spends it on exactly one shape, and the fixtures that matter
//! most are the ones proving it does not spend it on the others.

use std::collections::{BTreeSet, HashSet};

use arthron::UnresolvedReason::*;
use arthron::lang::Resolver;
use arthron::model::{Domain, NodeId, node_id};
use arthron::track_cpp::extract::extract;
use arthron::track_cpp::lang::{module_fqn, unit_fqn};
use arthron::track_cpp::project::CppProject;
use arthron::track_cpp::resolve::CppResolver;
use arthron::{Outcome, resolution_rate};

/// A project with the given include roots and unparsed headers sitting on
/// them.
fn project(roots: &[&str], unparsed: &[&str]) -> CppProject {
    CppProject {
        include_roots: roots.iter().map(|r| (*r).to_string()).collect(),
        unparsed: unparsed
            .iter()
            .map(|u| (*u).to_string())
            .collect::<BTreeSet<_>>(),
    }
}

/// A symbol table holding one unit node per repository-relative path, plus
/// one module node per named module.
fn table(units: &[&str], modules: &[&str]) -> HashSet<NodeId> {
    units
        .iter()
        .map(|f| node_id(Domain::Cxx, &unit_fqn(f)))
        .chain(modules.iter().map(|m| node_id(Domain::Cxx, &module_fqn(m))))
        .collect()
}

/// Every reference in one file, resolved.
fn outcomes(
    cfg: &CppProject,
    rel: &str,
    source: &str,
    units: &[&str],
    modules: &[&str],
) -> Vec<Outcome<NodeId, String>> {
    let known = table(units, modules);
    let facts = extract(rel, source);
    let scope = CppResolver.scope(cfg, &facts, &known);
    facts
        .refs
        .iter()
        .map(|r| CppResolver.resolve(cfg, &scope, r, &known).outcome)
        .collect()
}

/// The single outcome of a one-reference file.
fn only(
    cfg: &CppProject,
    rel: &str,
    source: &str,
    units: &[&str],
    modules: &[&str],
) -> Outcome<NodeId, String> {
    let mut got = outcomes(cfg, rel, source, units, modules);
    assert_eq!(got.len(), 1, "{got:?}");
    got.remove(0)
}

fn resolved_to(o: &Outcome<NodeId, String>, file: &str) -> bool {
    *o == Outcome::Resolved(node_id(Domain::Cxx, &unit_fqn(file)))
}

// ---------------------------------------------------------------------------
// One syntax, two roots
// ---------------------------------------------------------------------------

#[test]
fn a_quoted_include_starts_at_the_including_files_own_directory() {
    let cfg = project(&["include"], &[]);
    let got = only(
        &cfg,
        "test/format-test.cc",
        "#include \"util.hpp\"\n",
        &["test/util.hpp", "include/util.hpp"],
        &[],
    );
    assert!(resolved_to(&got, "test/util.hpp"), "{got:?}");
}

#[test]
fn a_quoted_include_falls_back_to_the_include_roots() {
    let cfg = project(&["include"], &[]);
    let got = only(
        &cfg,
        "test/format-test.cc",
        "#include \"fmt/ranges.hpp\"\n",
        &["include/fmt/ranges.hpp"],
        &[],
    );
    assert!(resolved_to(&got, "include/fmt/ranges.hpp"), "{got:?}");
}

#[test]
fn an_angled_include_never_starts_at_the_including_file() {
    // The sibling exists and the root copy does not. A resolver that tried
    // the file's own directory for `<...>` would resolve this, and would be
    // wrong about every system header in the corpus for the same reason.
    let cfg = project(&["include"], &[]);
    let got = only(
        &cfg,
        "test/format-test.cc",
        "#include <util.hpp>\n",
        &["test/util.hpp"],
        &[],
    );
    assert_eq!(got, Outcome::External("util.hpp".to_string()), "{got:?}");
}

#[test]
fn an_angled_include_resolves_an_in_repository_header_under_a_root() {
    let cfg = project(&["include"], &[]);
    let got = only(
        &cfg,
        "src/fmt-c.cc",
        "#include <fmt/base.hpp>\n",
        &["include/fmt/base.hpp"],
        &[],
    );
    assert!(resolved_to(&got, "include/fmt/base.hpp"), "{got:?}");
}

#[test]
fn a_relative_include_that_climbs_out_of_the_tree_resolves_to_nothing() {
    let cfg = project(&[], &[]);
    let got = only(
        &cfg,
        "src/os.cc",
        "#include \"../../elsewhere/x.hpp\"\n",
        &[],
        &[],
    );
    assert_eq!(got.unresolved_reason(), Some(&ModuleNotFound), "{got:?}");
}

#[test]
fn a_dotdot_include_reaches_a_sibling_directory() {
    let cfg = project(&["include"], &[]);
    let got = only(
        &cfg,
        "test/posix-mock-test.cc",
        "#include \"../src/os.cc\"\n",
        &["src/os.cc"],
        &[],
    );
    assert!(resolved_to(&got, "src/os.cc"), "{got:?}");
}

// ---------------------------------------------------------------------------
// What `External` is spent on, and what it is not
// ---------------------------------------------------------------------------

#[test]
fn a_system_header_under_no_include_root_is_external() {
    let cfg = project(&["include"], &["include/fmt/base.h"]);
    for header in ["vector", "sys/stat.h", "windows.h"] {
        let got = only(
            &cfg,
            "src/os.cc",
            &format!("#include <{header}>\n"),
            &[],
            &[],
        );
        assert_eq!(got, Outcome::External(header.to_string()), "{header}");
    }
}

#[test]
fn an_in_repository_header_this_build_does_not_parse_is_never_external() {
    // `include/fmt/base.h` is a real file in this repository. Calling it
    // `External` would move it outside *both* terms of the resolution rate —
    // the laundering the Rust review caught one language earlier — so it is a
    // floor that counts against the rate instead.
    let cfg = project(&["include"], &["include/fmt/base.h"]);
    let got = only(&cfg, "src/fmt-c.cc", "#include <fmt/base.h>\n", &[], &[]);
    assert_eq!(got.unresolved_reason(), Some(&ModuleNotFound), "{got:?}");
    assert!(!matches!(got, Outcome::External(_)));
}

#[test]
fn a_quoted_miss_is_a_floor_and_never_external() {
    // `"gtest/gtest.h"` names a bundle this corpus deliberately does not
    // vendor. The quoted syntax says this project supplies the header, so a
    // miss is the snapshot's own scope — which is the answer the PHP track
    // gave guzzle's out-of-snapshot vendor siblings, unchanged.
    let cfg = project(&["include"], &[]);
    for spec in ["gtest/gtest.h", "gmock/gmock.h", "fmt/format.h"] {
        let got = only(
            &cfg,
            "test/args-test.cc",
            &format!("#include \"{spec}\"\n"),
            &[],
            &[],
        );
        assert_eq!(got.unresolved_reason(), Some(&ModuleNotFound), "{spec}");
    }
}

// ---------------------------------------------------------------------------
// C++20 modules
// ---------------------------------------------------------------------------

#[test]
fn an_import_resolves_to_the_module_a_file_exports() {
    let cfg = project(&["include"], &[]);
    let got = only(&cfg, "test/module-test.cc", "import fmt;\n", &[], &["fmt"]);
    assert_eq!(
        got,
        Outcome::Resolved(node_id(Domain::Cxx, &module_fqn("fmt"))),
    );
}

#[test]
fn a_module_and_a_namespace_of_one_name_are_two_identities() {
    // fmt writes both `export module fmt;` and `namespace fmt`. A resolver
    // that shared their identity would resolve `import fmt;` to a namespace
    // and call it an edge.
    let cfg = project(&["include"], &[]);
    let namespace_only: HashSet<NodeId> = [node_id(Domain::Cxx, "fmt")].into_iter().collect();
    let facts = extract("test/module-test.cc", "import fmt;\n");
    let scope = CppResolver.scope(&cfg, &facts, &namespace_only);
    let got = CppResolver
        .resolve(&cfg, &scope, &facts.refs[0], &namespace_only)
        .outcome;
    assert_eq!(got.unresolved_reason(), Some(&UnknownPackage), "{got:?}");
}

#[test]
fn a_module_no_file_here_exports_is_an_unindexed_package() {
    let cfg = project(&["include"], &[]);
    let got = only(&cfg, "src/fmt.cc", "import std;\n", &[], &["fmt"]);
    assert_eq!(got.unresolved_reason(), Some(&UnknownPackage), "{got:?}");
}

// ---------------------------------------------------------------------------
// Nothing is dropped
// ---------------------------------------------------------------------------

#[test]
fn a_macro_specifier_resolves_against_nothing_and_says_so() {
    let cfg = project(&["include"], &[]);
    let got = only(
        &cfg,
        "src/os.cc",
        "#define H \"a.hpp\"\n#include H\n",
        &["src/a.hpp"],
        &[],
    );
    assert_eq!(
        got.unresolved_reason(),
        Some(&DynamicModuleSpecifier),
        "{got:?}",
    );
}

#[test]
fn every_reference_in_a_mixed_file_gets_exactly_one_outcome() {
    let cfg = project(&["include"], &["include/fmt/base.h"]);
    let source = concat!(
        "#include \"util.hpp\"\n",
        "#include \"fmt/ranges.hpp\"\n",
        "#include \"gtest/gtest.h\"\n",
        "#include <vector>\n",
        "#include <fmt/base.h>\n",
        "import fmt;\n",
        "#include SOME_MACRO\n",
    );
    let got = outcomes(
        &cfg,
        "test/x.cc",
        source,
        &["test/util.hpp", "include/fmt/ranges.hpp"],
        &["fmt"],
    );
    assert_eq!(got.len(), 7);
    let resolved = got.iter().filter(|o| o.is_resolved()).count() as u64;
    let external = got
        .iter()
        .filter(|o| matches!(o, Outcome::External(_)))
        .count();
    let unresolved = got.iter().filter_map(|o| o.unresolved_reason()).count() as u64;
    assert_eq!(resolved, 3, "{got:?}");
    assert_eq!(external, 1, "{got:?}");
    assert_eq!(unresolved, 3, "{got:?}");
    // Nothing dropped: every reference landed in exactly one bucket.
    assert_eq!(resolved + external as u64 + unresolved, got.len() as u64);
    assert_eq!(resolution_rate(resolved, unresolved), Some(0.5));
}
