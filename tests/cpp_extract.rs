//! The C++ extractor, one construct at a time.
//!
//! Two questions run through every case. What did the file *declare* — the
//! half of tier 2 no rate can see — and what did it *name*, which is the half
//! the gate measures. A third runs under both: an extractor that emitted a
//! call or a type use would put references into a denominator this track
//! cannot resolve, so every fixture below also asserts what is absent.

use arthron::model::{DefKind, RefKind};
use arthron::track_cpp::extract::{IncludeForm, extract};

/// Every definition as `kind name` under its owner, in source order, with the
/// synthetic unit node dropped — it is asserted on its own.
fn defs(source: &str) -> Vec<String> {
    let facts = extract("src/x.cc", source);
    facts.defs[1..]
        .iter()
        .map(|d| {
            let owner = d.owner.join("::");
            let name = if owner.is_empty() {
                d.name.clone()
            } else {
                format!("{owner}::{}", d.name)
            };
            format!("{} {name}", d.kind.name())
        })
        .collect()
}

/// Every import clause's form, in source order.
fn forms(rel: &str, source: &str) -> Vec<IncludeForm> {
    extract(rel, source)
        .header
        .includes
        .into_iter()
        .map(|i| i.form)
        .collect()
}

#[test]
fn every_file_declares_the_unit_an_include_names() {
    let facts = extract("include/fmt/os.hpp", "");
    assert_eq!(facts.defs.len(), 1);
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].name, "os.hpp");
    // A file that does not parse is still a file an `#include` can name.
    let broken = extract("src/broken.cc", "struct {{{ ;\n");
    assert_eq!(broken.defs[0].kind, DefKind::Module);
    assert_eq!(broken.defs[0].name, "broken.cc");
}

#[test]
fn the_two_include_syntaxes_are_two_forms() {
    assert_eq!(
        forms(
            "test/a.cc",
            "#include \"util.h\"\n#include \"fmt/format.h\"\n#include <vector>\n",
        ),
        [
            IncludeForm::Quoted("util.h".to_string()),
            IncludeForm::Quoted("fmt/format.h".to_string()),
            IncludeForm::Angle("vector".to_string()),
        ],
    );
}

#[test]
fn a_macro_specifier_is_never_guessed_at() {
    assert_eq!(
        forms("src/a.cc", "#define H <vector>\n#include H\n"),
        [IncludeForm::Computed],
    );
}

#[test]
fn no_preprocessor_branch_is_evaluated() {
    // Both arms are read. Picking one would make the measurement depend on a
    // platform nobody named, and a macro environment only a real compilation
    // has.
    let source = "#ifdef _WIN32\n#  include <windows.h>\n#else\n#  include <unistd.h>\n#endif\n";
    assert_eq!(
        forms("src/a.cc", source),
        [
            IncludeForm::Angle("windows.h".to_string()),
            IncludeForm::Angle("unistd.h".to_string()),
        ],
    );
}

#[test]
fn a_module_interface_declares_and_an_import_names() {
    let facts = extract("src/fmt.cc", "module;\nexport module fmt;\nimport std;\n");
    // `module;` opens the global module fragment. It names nothing and
    // declares nothing, and must not be read as either.
    let modules: Vec<&str> = facts.defs[1..].iter().map(|d| d.name.as_str()).collect();
    assert_eq!(modules, ["fmt"]);
    assert_eq!(facts.defs[1].kind, DefKind::Module);
    assert_eq!(
        forms("src/fmt.cc", "module;\nexport module fmt;\nimport std;\n"),
        [IncludeForm::Module("std".to_string())],
    );
}

#[test]
fn an_import_is_an_import_wherever_the_preprocessor_puts_it() {
    // fmt writes `import std;` inside `#ifdef FMT_IMPORT_STD`, and
    // `import fmt;` at the top level of its module test.
    assert_eq!(
        forms(
            "src/fmt.cc",
            "export module fmt;\n#ifdef FMT_IMPORT_STD\nimport std;\n#endif\n",
        ),
        [IncludeForm::Module("std".to_string())],
    );
    assert_eq!(
        forms("test/module-test.cc", "import fmt;\n#include \"x.h\"\n"),
        [
            IncludeForm::Module("fmt".to_string()),
            IncludeForm::Quoted("x.h".to_string()),
        ],
    );
}

#[test]
fn namespaces_nest_and_a_nested_specifier_is_several_frames() {
    assert_eq!(
        defs("namespace fmt { namespace detail { } }\n"),
        ["module fmt", "module fmt::detail"],
    );
    assert_eq!(
        defs("namespace fmt::inline v11 { struct A { }; }\n"),
        ["module fmt::v11", "type fmt::v11::A"],
    );
}

#[test]
fn an_unnamed_namespace_names_nothing_and_neither_do_its_contents() {
    assert_eq!(
        defs("namespace { void hidden() {} }\n"),
        Vec::<String>::new()
    );
}

#[test]
fn records_enumerations_and_their_constants() {
    assert_eq!(
        defs("struct S { }; class C { }; union U { }; enum class E { X, Y };\n"),
        [
            "type S",
            "type C",
            "type U",
            "type E",
            "const E::X",
            "const E::Y"
        ],
    );
}

#[test]
fn member_functions_are_structure_and_data_members_are_not() {
    assert_eq!(
        defs(
            "class C {\n  int field_;\n  static int shared_;\n public:\n  C();\n  ~C();\n  void m();\n  int inline_m() { return 0; }\n};\n"
        ),
        [
            "type C",
            "constructor C::C",
            "method C::~C",
            "method C::m",
            "method C::inline_m",
        ],
    );
}

#[test]
fn a_free_function_a_prototype_and_an_out_of_line_definition() {
    assert_eq!(
        defs("namespace fmt {\nvoid go();\nvoid go() {}\n}\nvoid fmt::detail::later() {}\n"),
        [
            "module fmt",
            "function fmt::go",
            "function fmt::go",
            // One file cannot say whether `detail` is a class or a
            // namespace, so the owner is claimed and the kind is not.
            "function fmt::detail::later",
        ],
    );
}

#[test]
fn a_declaration_inside_a_function_body_has_no_lexical_owner() {
    assert_eq!(
        defs("void f() {\n  struct Local { };\n  static int counter = 0;\n}\n"),
        ["function f"],
    );
}

#[test]
fn aliases_of_all_three_spellings() {
    assert_eq!(
        defs(
            "namespace fmt {\ntypedef int myint;\nusing alias_t = double;\nnamespace ns = fmt::detail;\n}\n"
        ),
        [
            "module fmt",
            "alias fmt::myint",
            "alias fmt::alias_t",
            "alias fmt::ns",
        ],
    );
}

#[test]
fn a_template_declares_the_thing_it_wraps() {
    assert_eq!(
        defs("template <typename T> struct buffer { };\ntemplate <typename T> void put(T v) {}\n"),
        ["type buffer", "function put"],
    );
}

#[test]
fn nothing_a_tier_two_extractor_emits_is_a_call_or_a_type_use() {
    let source = "#include \"a.h\"\nstruct Base { };\nstruct D : Base { void m() { helper(1); } };\nD make() { return D(); }\n";
    let facts = extract("src/x.cc", source);
    for r in &facts.refs {
        assert_eq!(r.kind, RefKind::Import, "{}", r.raw_target);
        assert!(!r.locally_bound, "{}", r.raw_target);
    }
    assert_eq!(facts.refs.len(), 1);
}

#[test]
fn every_clause_is_paired_with_a_reference_and_both_are_in_source_order() {
    let source = "#include <a.h>\nnamespace fmt {\n#include \"b.h\"\n}\nimport c;\n#include D\n";
    let facts = extract("src/x.cc", source);
    assert_eq!(facts.header.includes.len(), 4);
    assert_eq!(facts.refs.len(), facts.header.includes.len());
    for (clause, r) in facts.header.includes.iter().zip(&facts.refs) {
        assert_eq!(
            clause.span, r.span,
            "a clause and its reference must share a span"
        );
    }
    let lines: Vec<u32> = facts.refs.iter().map(|r| r.span.line).collect();
    assert_eq!(lines, [1, 3, 5, 6]);
}

#[test]
fn an_include_inside_a_namespace_is_sourced_at_that_namespace() {
    let source = "namespace fmt {\n#include \"b.h\"\n}\n";
    let facts = extract("src/x.cc", source);
    let enclosing = facts.refs[0].enclosing.clone().expect("an encloser");
    assert_eq!(enclosing.path, ["fmt"]);
    assert_eq!(enclosing.kind, DefKind::Module);
    // At the top of a file an include belongs to nothing, and the driver
    // sources it at the file's own unit node.
    let top = extract("src/x.cc", "#include \"b.h\"\n");
    assert!(top.refs[0].enclosing.is_none());
}

#[test]
fn a_macro_invocation_is_not_a_function_definition() {
    // `TEST(suite, case) { … }` is a `function_definition` named `TEST` to
    // this grammar, and the corpus writes 600 of them. C++ gives every
    // function a declared return type except a constructor, a destructor and
    // a conversion function, which is the rule that tells the two apart —
    // and merging 600 of them into one `TEST` identity would be the worse
    // half of the same mistake.
    assert_eq!(
        defs("TEST(format_test, escape) { int x = 1; }\n"),
        Vec::<String>::new(),
    );
    // The three shapes the rule must not throw out with it.
    assert_eq!(
        defs("struct A { A(); ~A(); };\nA::A() {}\nA::~A() {}\nvoid f() {}\n"),
        [
            "type A",
            "constructor A::A",
            "method A::~A",
            "constructor A::A",
            "method A::~A",
            "function f",
        ],
    );
}

#[test]
fn a_macro_standing_where_a_return_type_would_be_is_indistinguishable() {
    // The other half of not running a preprocessor, asserted rather than
    // left to be discovered from a census. `FMT_END_NAMESPACE` expands to
    // nothing and carries no semicolon, so the grammar reads it as the return
    // type of the `TEST` that follows. 24 of the corpus's function
    // definitions are this, and without a macro environment there is nothing
    // left to tell them from a function returning `FMT_END_NAMESPACE`.
    assert_eq!(
        defs("FMT_END_NAMESPACE\nTEST(args_test, custom) { }\n"),
        ["function TEST"],
    );
}
