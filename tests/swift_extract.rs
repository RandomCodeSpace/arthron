//! Swift extractor fixtures: one construct at a time, on source small enough
//! that the expected record set can be written out in full.
//!
//! The tier-2 contract is asserted here rather than assumed: the only
//! reference kind this extractor may emit is [`RefKind::Import`], and every
//! definition it emits is one a Swift programmer could write a name for.
//! `tests/swift_corpus.rs` checks the same two claims over 91 real files;
//! these check them over the shapes a corpus may or may not happen to
//! contain.

use arthron::model::{DefFacets, DefKind, RefKind};
use arthron::track_swift::extract::extract;

/// Every definition, as `(kind, owner-joined, name, line)`.
fn defs(source: &str) -> Vec<(DefKind, String, String, u32)> {
    extract("Source/X.swift", source)
        .defs
        .iter()
        .map(|d| (d.kind, d.owner.join("."), d.name.clone(), d.span.line))
        .collect()
}

/// Every definition below the file's own synthetic module placeholder.
fn written(source: &str) -> Vec<(DefKind, String, String, u32)> {
    defs(source).split_off(1)
}

#[test]
fn a_file_declares_its_module_without_naming_it() {
    // The whole of Swift's resolution problem in one assertion: module
    // membership is decided by the package manifest, so the *file* states no
    // module name at all and the placeholder carries an empty one.
    let facts = extract("Source/Core/Session.swift", "class Session {}\n");
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].name, "");
    assert!(facts.defs[0].facets.contains(DefFacets::SYNTHETIC));
    assert!(facts.defs[0].owner.is_empty());
}

#[test]
fn the_five_type_declaration_keywords_are_all_types() {
    assert_eq!(
        written("class C {}\nstruct S {}\nenum E {}\nactor A {}\nprotocol P {}\n"),
        [
            (DefKind::Type, String::new(), "C".to_string(), 1),
            (DefKind::Type, String::new(), "S".to_string(), 2),
            (DefKind::Type, String::new(), "E".to_string(), 3),
            (DefKind::Type, String::new(), "A".to_string(), 4),
            (DefKind::Type, String::new(), "P".to_string(), 5),
        ],
    );
}

#[test]
fn an_extension_declares_no_type_and_its_members_belong_to_the_extended_one() {
    // The corpus's 194 extensions are the reason this test exists. An
    // extension is not a declaration of the type it extends — saying it was
    // would put `URLRequest`, which Foundation owns, into the repository's
    // own definition table.
    let out = written("extension URLRequest {\n  func af() {}\n  var x: Int { 0 }\n}\n");
    assert_eq!(
        out,
        [
            (
                DefKind::Method,
                "URLRequest".to_string(),
                "af()".to_string(),
                2,
            ),
            (
                DefKind::Property,
                "URLRequest".to_string(),
                "x".to_string(),
                3,
            ),
        ],
    );
    // The extension itself is still counted, because nothing else in the
    // record set can see it.
    let facts = extract("Source/X.swift", "extension URLRequest { func af() {} }\n");
    assert_eq!(facts.header.extensions.len(), 1);
    assert_eq!(facts.header.extensions[0].extended, ["URLRequest"]);
}

#[test]
fn an_extension_of_a_nested_type_keeps_the_whole_path() {
    assert_eq!(
        written("extension Outer.Inner {\n  func m() {}\n}\n"),
        [(
            DefKind::Method,
            "Outer.Inner".to_string(),
            "m()".to_string(),
            2,
        )],
    );
}

#[test]
fn a_callable_is_named_the_way_swift_names_a_declaration() {
    // Argument labels are part of a Swift declaration's name: `request(_:)`
    // and `request(url:)` are two declarations, and one node for both would
    // be a census that under-counts the API surface it claims to measure.
    assert_eq!(
        written(
            "func f() {}\nfunc g(_ a: Int) {}\nfunc h(with b: Int, and c: Int) {}\n\
             func i(x: Int) {}\n"
        ),
        [
            (DefKind::Function, String::new(), "f()".to_string(), 1),
            (DefKind::Function, String::new(), "g(_:)".to_string(), 2),
            (
                DefKind::Function,
                String::new(),
                "h(with:and:)".to_string(),
                3,
            ),
            (DefKind::Function, String::new(), "i(x:)".to_string(), 4),
        ],
    );
}

#[test]
fn a_function_in_a_type_is_a_method_and_a_static_one_says_so() {
    let facts = extract(
        "Source/X.swift",
        "struct S {\n  func m() {}\n  static func s() {}\n  class func c() {}\n}\n",
    );
    let members: Vec<(&str, bool)> = facts.defs[1..]
        .iter()
        .filter(|d| d.kind == DefKind::Method)
        .map(|d| (d.name.as_str(), d.facets.contains(DefFacets::STATIC)))
        .collect();
    assert_eq!(members, [("m()", false), ("s()", true), ("c()", true)]);
}

#[test]
fn a_class_is_a_type_and_not_a_static_anything() {
    // `class func` is `static`, and the keyword sits outside the modifier
    // list — so the detector that reads it has to tell that keyword from the
    // `class` in `class Foo {}`, which is the declaration's own kind. Reading
    // the second as the first stamped `STATIC` on every class in the graph:
    // 174 of the measured corpus's 383 type nodes carried the false fact, and
    // no test could see it because the only `STATIC` assertion nearby is
    // about methods inside a `struct`.
    let facts = extract(
        "Source/X.swift",
        "public class Foo {\n  class func c() {}\n}\nfinal class Q {}\n\
         struct S {}\nenum E {}\nactor A {}\nprotocol P {}\n",
    );
    let types: Vec<(&str, bool)> = facts
        .defs
        .iter()
        .filter(|d| d.kind == DefKind::Type)
        .map(|d| (d.name.as_str(), d.facets.contains(DefFacets::STATIC)))
        .collect();
    assert_eq!(
        types,
        [
            ("Foo", false),
            ("Q", false),
            ("S", false),
            ("E", false),
            ("A", false),
            ("P", false),
        ],
    );
    // And the member that really is one still says so, from inside the very
    // class whose own keyword must not be read as its modifier.
    let member = facts
        .defs
        .iter()
        .find(|d| d.name == "c()")
        .expect("the class func");
    assert!(member.facets.contains(DefFacets::STATIC));
}

#[test]
fn initialisers_deinitialisers_and_subscripts_are_named_not_skipped() {
    assert_eq!(
        written(
            "class C {\n  init() {}\n  init?(url: Int) {}\n  deinit {}\n  \
             subscript(i: Int) -> Int { 0 }\n}\n"
        ),
        [
            (DefKind::Type, String::new(), "C".to_string(), 1),
            (
                DefKind::Constructor,
                "C".to_string(),
                "init()".to_string(),
                2,
            ),
            (
                DefKind::Constructor,
                "C".to_string(),
                "init(url:)".to_string(),
                3,
            ),
            (DefKind::Method, "C".to_string(), "deinit".to_string(), 4),
            (
                DefKind::Property,
                "C".to_string(),
                "subscript(i:)".to_string(),
                5,
            ),
        ],
    );
}

#[test]
fn a_stored_property_is_a_field_and_a_computed_one_is_a_property() {
    assert_eq!(
        written(
            "struct S {\n  var a = 0\n  let b: Int = 1\n  var c: Int { 2 }\n  \
             var d = 0 { didSet {} }\n}\n"
        ),
        [
            (DefKind::Type, String::new(), "S".to_string(), 1),
            (DefKind::Field, "S".to_string(), "a".to_string(), 2),
            (DefKind::Field, "S".to_string(), "b".to_string(), 3),
            (DefKind::Property, "S".to_string(), "c".to_string(), 4),
            // An observer is not an accessor: the storage is still there.
            (DefKind::Field, "S".to_string(), "d".to_string(), 5),
        ],
    );
}

#[test]
fn a_top_level_binding_is_a_constant_or_a_variable() {
    assert_eq!(
        written("let a = 1\nvar b = 2\nlet c = 3, d = 4\nlet _ = 5\n"),
        [
            (DefKind::Const, String::new(), "a".to_string(), 1),
            (DefKind::Var, String::new(), "b".to_string(), 2),
            (DefKind::Const, String::new(), "c".to_string(), 3),
            (DefKind::Const, String::new(), "d".to_string(), 3),
            // `_` binds nothing a reference could name.
        ],
    );
}

#[test]
fn every_name_in_one_case_clause_is_its_own_constant() {
    assert_eq!(
        written("enum E {\n  case a\n  case b, c\n  case d(Int)\n  case e = 1\n}\n"),
        [
            (DefKind::Type, String::new(), "E".to_string(), 1),
            (DefKind::Const, "E".to_string(), "a".to_string(), 2),
            (DefKind::Const, "E".to_string(), "b".to_string(), 3),
            (DefKind::Const, "E".to_string(), "c".to_string(), 3),
            (DefKind::Const, "E".to_string(), "d".to_string(), 4),
            (DefKind::Const, "E".to_string(), "e".to_string(), 5),
        ],
    );
}

#[test]
fn a_protocol_states_requirements_and_they_are_declarations_too() {
    let facts = extract(
        "Source/X.swift",
        "protocol P {\n  associatedtype T\n  var v: Int { get }\n  func f(a: Int)\n  init()\n}\n",
    );
    let out: Vec<(DefKind, String, bool)> = facts.defs[1..]
        .iter()
        .map(|d| {
            (
                d.kind,
                d.name.clone(),
                d.facets.contains(DefFacets::ABSTRACT),
            )
        })
        .collect();
    assert_eq!(
        out,
        [
            (DefKind::Type, "P".to_string(), false),
            (DefKind::Type, "T".to_string(), true),
            (DefKind::Property, "v".to_string(), true),
            (DefKind::Method, "f(a:)".to_string(), true),
            (DefKind::Constructor, "init()".to_string(), true),
        ],
    );
    assert!(facts.defs[1].facets.contains(DefFacets::INTERFACE));
}

#[test]
fn a_typealias_is_an_alias_and_an_enum_says_it_is_one() {
    let out = written("typealias T = Int\nenum E {}\n");
    assert_eq!(out[0], (DefKind::Alias, String::new(), "T".to_string(), 1));
    let facts = extract("Source/X.swift", "enum E {}\n");
    assert!(facts.defs[1].facets.contains(DefFacets::ENUM));
}

#[test]
fn visibility_is_recorded_and_a_private_setter_is_not_a_private_declaration() {
    let facts = extract(
        "Source/X.swift",
        "public struct S {\n  private var a = 0\n  public private(set) var b = 0\n  \
         fileprivate func c() {}\n}\n",
    );
    let out: Vec<(&str, bool, bool)> = facts.defs[1..]
        .iter()
        .map(|d| {
            (
                d.name.as_str(),
                d.facets.contains(DefFacets::EXPORTED),
                d.facets.contains(DefFacets::PRIVATE),
            )
        })
        .collect();
    assert_eq!(
        out,
        [
            ("S", true, false),
            ("a", false, true),
            // `private(set)` narrows the setter, not the declaration.
            ("b", true, false),
            ("c()", false, true),
        ],
    );
}

#[test]
fn a_declaration_inside_a_body_is_not_nameable_and_is_not_emitted() {
    // A `let` in a function body is a local, a `func` in one is a closure by
    // another spelling, and neither is a node any other file can name.
    assert_eq!(
        written(
            "func outer() {\n  let a = 1\n  func inner() {}\n  struct Local {}\n}\n\
             struct S {\n  var p: Int { let t = 1; return t }\n}\n"
        ),
        [
            (DefKind::Function, String::new(), "outer()".to_string(), 1,),
            (DefKind::Type, String::new(), "S".to_string(), 6),
            (DefKind::Property, "S".to_string(), "p".to_string(), 7),
        ],
    );
}

#[test]
fn both_arms_of_a_conditional_compilation_block_are_read_as_written() {
    // 74 `#if` blocks in the corpus, and the graph is the union over
    // configurations: a declaration that exists under *some* platform is a
    // declaration, and arthron neither picks a platform nor drops an arm.
    assert_eq!(
        written(
            "#if canImport(Security)\nfunc a() {}\n#else\nfunc b() {}\n#endif\n\
             struct S {\n#if os(iOS)\n  func c() {}\n#else\n  func d() {}\n#endif\n}\n"
        ),
        [
            (DefKind::Function, String::new(), "a()".to_string(), 2),
            (DefKind::Function, String::new(), "b()".to_string(), 4),
            (DefKind::Type, String::new(), "S".to_string(), 6),
            (DefKind::Method, "S".to_string(), "c()".to_string(), 8),
            (DefKind::Method, "S".to_string(), "d()".to_string(), 10),
        ],
    );
}

#[test]
fn only_import_references_are_emitted() {
    let facts = extract(
        "Tests/T.swift",
        "import Foundation\n@testable import Alamofire\n\
         class C: Base {\n  func f() { g(); let x: Int = 0 }\n}\n",
    );
    // A call, a type use and a base class are all present in the source and
    // none of them is a reference: tier 2 resolves imports, so emitting them
    // would report a denominator nothing in this track can link.
    assert_eq!(facts.refs.len(), 2);
    for r in &facts.refs {
        assert_eq!(r.kind, RefKind::Import);
        assert!(!r.locally_bound);
        assert!(r.enclosing.is_none());
    }
    assert_eq!(facts.refs[0].raw_target, "import Foundation");
    assert_eq!(facts.refs[1].raw_target, "@testable import Alamofire");
}

#[test]
fn an_import_carries_its_module_path_and_its_testable_flag() {
    let facts = extract(
        "Tests/T.swift",
        "import Foundation\n@testable\nimport Alamofire\n\
         @_spi(WebSocket) import Alamofire\nimport struct Foundation.Data\n",
    );
    let paths: Vec<(Vec<String>, bool)> = facts
        .header
        .imports
        .iter()
        .map(|i| (i.path.clone(), i.testable))
        .collect();
    assert_eq!(
        paths,
        [
            (vec!["Foundation".to_string()], false),
            // The attribute may sit on its own line; the parse decides, not
            // the line break.
            (vec!["Alamofire".to_string()], true),
            (vec!["Alamofire".to_string()], false),
            (vec!["Foundation".to_string(), "Data".to_string()], false,),
        ],
    );
    // Every clause is paired with exactly one reference, by span.
    assert_eq!(facts.header.imports.len(), facts.refs.len());
    for (spec, r) in facts.header.imports.iter().zip(&facts.refs) {
        assert_eq!(spec.span.byte_start, r.span.byte_start);
    }
}

#[test]
fn records_come_out_in_source_order() {
    let facts = extract(
        "Source/X.swift",
        "import A\nstruct S {\n  func m() {}\n}\nimport B\n",
    );
    assert_eq!(
        facts.refs.iter().map(|r| r.span.line).collect::<Vec<_>>(),
        [1, 5],
    );
    assert!(
        facts.defs[1..]
            .windows(2)
            .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
        "{:?}",
        facts.defs,
    );
}

#[test]
fn a_file_that_does_not_parse_still_declares_its_module() {
    let facts = extract("Source/broken.swift", "class ((( \n");
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert!(facts.defs[0].facets.contains(DefFacets::SYNTHETIC));
}
