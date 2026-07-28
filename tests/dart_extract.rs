//! Extractor fixtures for Dart: what one file yields, and what it must not.
//!
//! Two halves. The first pins the **definition census** a real library
//! produces, because tier 2's deliverable is half definitions and no rate can
//! see them. The second pins the **tier-2 contract**: no call reference, no
//! type reference, no supertype reference, whatever the source writes.

use std::collections::BTreeMap;

use arthron::model::{DeclSpace, DefFacets, DefKind, RefKind};
use arthron::track_dart::extract::{UriForm, extract};

/// Every definition the extractor emits, as `(kind, dotted name)`.
fn defs(rel: &str, source: &str) -> Vec<(DefKind, String)> {
    extract(rel, source)
        .defs
        .iter()
        .map(|d| {
            let mut name = d.owner.join(".");
            if !name.is_empty() {
                name.push('.');
            }
            name.push_str(&d.name);
            (d.kind, name)
        })
        .collect()
}

/// Every reference the extractor emits, as `(kind, raw target)`.
fn refs(rel: &str, source: &str) -> Vec<(RefKind, String)> {
    extract(rel, source)
        .refs
        .iter()
        .map(|r| (r.kind, r.raw_target.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// The library node
// ---------------------------------------------------------------------------

#[test]
fn every_file_declares_the_library_an_import_names_first() {
    // The driver reads the first `Module` definition as the file's container,
    // so it has to be first, and it has to exist for a file that declares
    // nothing at all: an `import` naming an empty library still resolves.
    let facts = extract("lib/src/empty.dart", "");
    assert_eq!(facts.defs.len(), 1);
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].name, "empty");
    assert_eq!(facts.defs[0].space, DeclSpace::Namespace);
    assert!(facts.defs[0].facets.contains(DefFacets::SYNTHETIC));
}

#[test]
fn a_broken_file_still_yields_its_library_node() {
    // tree-sitter is error-tolerant, and a file that does not parse is still a
    // file an `import` can name.
    let facts = extract("lib/broken.dart", "class ((( \n");
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].name, "broken");
}

// ---------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------

#[test]
fn a_class_yields_its_members_under_it() {
    let got = defs(
        "lib/src/wrappers.dart",
        "abstract class DelegatingList<E> extends Base implements List<E> {\n\
         \x20 final List<E> _base;\n\
         \x20 static const int max = 3;\n\
         \x20 const DelegatingList(this._base);\n\
         \x20 DelegatingList.of(this._base);\n\
         \x20 factory DelegatingList.from(Iterable<E> s) => DelegatingList(s.toList());\n\
         \x20 List<E> get base => _base;\n\
         \x20 set base(List<E> v) {}\n\
         \x20 void add(E value) {}\n\
         \x20 static void helper() {}\n\
         \x20 E operator [](int i) => _base[i];\n\
         \x20 void abstractMember();\n\
         }\n",
    );
    assert_eq!(
        got,
        vec![
            (DefKind::Module, "wrappers".to_string()),
            (DefKind::Type, "DelegatingList".to_string()),
            (DefKind::Field, "DelegatingList._base".to_string()),
            (DefKind::Field, "DelegatingList.max".to_string()),
            // Dart's own tear-off spelling for the unnamed constructor.
            (DefKind::Constructor, "DelegatingList.new".to_string()),
            (DefKind::Constructor, "DelegatingList.of".to_string()),
            (DefKind::Constructor, "DelegatingList.from".to_string()),
            (DefKind::Property, "DelegatingList.base".to_string()),
            (DefKind::Property, "DelegatingList.base".to_string()),
            (DefKind::Method, "DelegatingList.add".to_string()),
            (DefKind::Method, "DelegatingList.helper".to_string()),
            (DefKind::Method, "DelegatingList.[]".to_string()),
            (DefKind::Method, "DelegatingList.abstractMember".to_string()),
        ],
    );
}

#[test]
fn the_facets_a_declaration_carries_are_read_from_the_declaration() {
    let facts = extract(
        "lib/a.dart",
        "abstract class A {\n  static int s = 0;\n  int _private = 1;\n  void gone();\n}\n",
    );
    let by_name: BTreeMap<&str, DefFacets> = facts
        .defs
        .iter()
        .map(|d| (d.name.as_str(), d.facets))
        .collect();
    assert!(by_name["A"].contains(DefFacets::ABSTRACT));
    assert!(by_name["A"].contains(DefFacets::EXPORTED));
    assert!(by_name["s"].contains(DefFacets::STATIC));
    // Dart spells visibility in the name, and a leading `_` is private to the
    // library.
    assert!(!by_name["_private"].contains(DefFacets::EXPORTED));
    // No body, so abstract — which the grammar says structurally.
    assert!(by_name["gone"].contains(DefFacets::ABSTRACT));
}

#[test]
fn one_declaration_per_declared_name_even_when_a_line_declares_two() {
    let got = defs(
        "lib/a.dart",
        "int a1 = 1, a2 = 2;\nconst k = 3;\nfinal f = 4;\n",
    );
    assert_eq!(
        got,
        vec![
            (DefKind::Module, "a".to_string()),
            (DefKind::Var, "a1".to_string()),
            (DefKind::Var, "a2".to_string()),
            (DefKind::Const, "k".to_string()),
            // `final` is not `const` in Dart: it is initialised once at run
            // time, so it is a variable and not a compile-time constant.
            (DefKind::Var, "f".to_string()),
        ],
    );
}

#[test]
fn every_type_level_declaration_dart_has_is_a_type_node() {
    // None of these appears in the measured corpus; each is fixture-proven so
    // that the first repository writing one is measured rather than dropped.
    let got = defs(
        "lib/a.dart",
        "mixin M on Base {\n  void m() {}\n}\n\
         enum Colour { red, green }\n\
         extension IterExt<T> on Iterable<T> {\n  T? get firstOrNull => null;\n}\n\
         extension type Meters(int v) {\n  int get squared => v * v;\n}\n\
         typedef Predicate<T> = bool Function(T);\n",
    );
    assert_eq!(
        got,
        vec![
            (DefKind::Module, "a".to_string()),
            (DefKind::Type, "M".to_string()),
            (DefKind::Method, "M.m".to_string()),
            (DefKind::Type, "Colour".to_string()),
            (DefKind::Const, "Colour.red".to_string()),
            (DefKind::Const, "Colour.green".to_string()),
            (DefKind::Type, "IterExt".to_string()),
            (DefKind::Property, "IterExt.firstOrNull".to_string()),
            (DefKind::Type, "Meters".to_string()),
            (DefKind::Property, "Meters.squared".to_string()),
            // A `typedef` is a named type, not a `DefKind::Alias`: an alias
            // node promises a forward to the definition it names, and that
            // name is a type use this tier does not resolve.
            (DefKind::Type, "Predicate".to_string()),
        ],
    );
}

#[test]
fn an_enum_carries_its_constants_and_its_members() {
    let got = defs(
        "lib/a.dart",
        "enum Suit {\n  hearts('h'), spades('s');\n  const Suit(this.c);\n  final String c;\n}\n",
    );
    assert_eq!(
        got,
        vec![
            (DefKind::Module, "a".to_string()),
            (DefKind::Type, "Suit".to_string()),
            (DefKind::Const, "Suit.hearts".to_string()),
            (DefKind::Const, "Suit.spades".to_string()),
            (DefKind::Constructor, "Suit.new".to_string()),
            (DefKind::Field, "Suit.c".to_string()),
        ],
    );
}

#[test]
fn a_declaration_inside_a_body_is_a_local_and_not_a_node() {
    // A local function and a local variable are real declarations Dart scopes
    // to that body. Locals are not nodes by decision.
    let got = defs(
        "lib/a.dart",
        "void outer() {\n  var local = 1;\n  int inner() => 2;\n  final closure = () { var deep = 3; };\n}\n",
    );
    assert_eq!(
        got,
        vec![
            (DefKind::Module, "a".to_string()),
            (DefKind::Function, "outer".to_string()),
        ],
    );
}

#[test]
fn an_unnamed_extension_names_nothing_and_neither_do_its_members() {
    let got = defs(
        "lib/a.dart",
        "extension on Iterable<int> {\n  int get total => 0;\n  void go() {}\n}\n",
    );
    assert_eq!(got, vec![(DefKind::Module, "a".to_string())]);
}

#[test]
fn records_come_out_in_source_order() {
    let facts = extract(
        "lib/a.dart",
        "import 'b.dart';\nclass A {}\nvoid f() {}\nexport 'c.dart';\n",
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
            .refs
            .windows(2)
            .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
        "{:?}",
        facts.refs,
    );
}

// ---------------------------------------------------------------------------
// Directives
// ---------------------------------------------------------------------------

#[test]
fn the_four_directives_that_name_a_uri_all_emit_one() {
    let got = refs(
        "lib/a.dart",
        "library;\nimport 'dart:math' as math;\nexport 'src/b.dart' show B;\npart 'a_part.dart';\n",
    );
    assert_eq!(
        got,
        vec![
            (RefKind::Import, "import 'dart:math' as math".to_string()),
            // An export is a re-export and gets its own kind, so a file that
            // both imports and exports one library keeps two rows.
            (RefKind::Export, "export 'src/b.dart'".to_string()),
            (RefKind::Import, "part 'a_part.dart'".to_string()),
        ],
    );
}

#[test]
fn a_part_of_directive_emits_only_for_the_uri_spelling() {
    assert_eq!(
        refs("lib/a.dart", "part of 'parent.dart';\n"),
        vec![(RefKind::Import, "part of 'parent.dart'".to_string())],
    );
    // The legacy spelling names a library by its declared name, which this
    // track does not index. Recorded as an under-count, never guessed.
    assert!(refs("lib/a.dart", "part of my.parent;\n").is_empty());
}

#[test]
fn a_configurable_import_names_two_libraries_and_emits_two_references() {
    let got = refs(
        "lib/a.dart",
        "import 'stub.dart' if (dart.library.io) 'io.dart' if (dart.library.js) 'web.dart';\n",
    );
    assert_eq!(
        got,
        vec![
            (RefKind::Import, "import 'stub.dart'".to_string()),
            (RefKind::Import, "import 'io.dart'".to_string()),
            (RefKind::Import, "import 'web.dart'".to_string()),
        ],
    );
}

#[test]
fn two_imports_of_one_library_under_two_prefixes_stay_two_rows() {
    // The raw target is the store's dedup key component; if the prefix were
    // dropped from it the second import would vanish into the first.
    let got = refs(
        "lib/a.dart",
        "import 'b.dart' as first;\nimport 'b.dart' as second;\n",
    );
    assert_eq!(
        got,
        vec![
            (RefKind::Import, "import 'b.dart' as first".to_string()),
            (RefKind::Import, "import 'b.dart' as second".to_string()),
        ],
    );
}

#[test]
fn adjacent_string_literals_are_one_uri_and_an_interpolation_is_none() {
    let facts = extract(
        "lib/a.dart",
        "import 'src/' 'b.dart';\nimport '${p}/c.dart';\n",
    );
    let forms: Vec<&UriForm> = facts.header.uris.iter().map(|u| &u.form).collect();
    assert_eq!(
        forms,
        vec![
            &UriForm::Literal("src/b.dart".to_string()),
            &UriForm::Dynamic
        ],
    );
}

#[test]
fn a_combinator_is_structure_and_never_a_reference() {
    // `show A, B` names two declarations inside another library, and pricing
    // one means computing that library's exported name set. Recorded, not
    // referenced — a name honestly not counted beats a name guessed at.
    let facts = extract(
        "lib/a.dart",
        "import 'b.dart' show Alpha, Beta hide Gamma;\n",
    );
    assert_eq!(facts.refs.len(), 1);
    assert_eq!(facts.header.uris[0].combinators, ["Alpha", "Beta", "Gamma"],);
}

// ---------------------------------------------------------------------------
// The tier-2 contract
// ---------------------------------------------------------------------------

#[test]
fn nothing_but_a_uri_is_ever_a_reference() {
    // Every shape a tier-1 extractor would turn into a reference, in one
    // file. A tier-2 track that emitted them would put references into a
    // denominator nothing here resolves and claim coverage it never measured.
    let facts = extract(
        "lib/a.dart",
        "import 'b.dart';\n\
         class C extends Base with Mixin implements Iface {\n\
         \x20 @override\n\
         \x20 void go() {\n\
         \x20   helper();\n\
         \x20   final x = Other();\n\
         \x20   x.method();\n\
         \x20 }\n\
         }\n",
    );
    assert_eq!(facts.refs.len(), 1, "{:?}", facts.refs);
    for r in &facts.refs {
        assert!(matches!(r.kind, RefKind::Import | RefKind::Export));
        assert!(!r.locally_bound);
        assert_eq!(r.space, DeclSpace::Namespace);
        // A directive sits inside no declaration, so the driver sources its
        // edge at the library node — which is exactly what imports it.
        assert!(r.enclosing.is_none());
        assert_eq!(r.argc, None);
    }
}

#[test]
fn a_uri_record_and_its_reference_share_a_span_one_to_one() {
    let facts = extract(
        "lib/a.dart",
        "import 'a.dart' if (dart.library.io) 'b.dart';\nexport 'c.dart';\n",
    );
    assert_eq!(facts.header.uris.len(), facts.refs.len());
    for (spec, r) in facts.header.uris.iter().zip(&facts.refs) {
        assert_eq!(spec.span, r.span);
    }
}
