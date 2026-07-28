//! Go type uses: every written type name is a reference, and each one gets an
//! honest outcome.
//!
//! Until this file existed the Go extractor emitted `Call` and `Import` and
//! nothing else. Every other tier-1 language emits type uses, so "tier 1" did
//! not mean the same thing in Go as in Java, TypeScript or Python: Go's
//! denominator was about a third of its reference surface where TypeScript's
//! was four fifths, and the README's tier-1 claim was false for Go.
//!
//! The cases below are one per *position* the Go grammar can write a type in,
//! because that inventory is the change — not a count. Each asserts the
//! definition the reference reaches by name, so a type use linked to the wrong
//! package is a failure here rather than one more `resolved`.
//!
//! The three outcomes a type use can honestly take:
//!
//! - `Resolved` — a type declared in this repository.
//! - `External` — a type in a declared dependency, or one of Go's predeclared
//!   universe names (`int`, `error`, `any`), which name nothing in any
//!   repository and are reported as `go:builtin` exactly as the predeclared
//!   *functions* already were.
//! - `LocalBinding` — a type parameter, or a `type` declared inside a function
//!   body. Neither is nameable from outside the block that declares it, so
//!   neither is a node, and the policy on
//!   [`arthron::UnresolvedReason::LocalBinding`] puts both outside the rate.
//!
//! No new `UnresolvedReason` was needed for any of it, which is the test that
//! the taxonomy already covered this surface.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use arthron::model::{RefKind, reason_name};
use arthron::pipeline::scan_go;
use arthron::store::{NodeRecord, Store, StoredOutcome};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Every reference row of one scan, rendered as the *name* it points at.
struct Scan {
    rows: Vec<(String, u8, String)>,
}

fn render(files: &[(&str, &str)]) -> Scan {
    let dir = tempfile::tempdir().expect("a scratch directory");
    for (path, source) in files {
        write(dir.path(), path, source);
    }
    let db = dir.path().join("graph.redb");
    scan_go(dir.path(), &db).expect("scan");
    let store = Store::open(&db).expect("the store opens");
    let snapshot = store.snapshot().expect("snapshot");
    let names: BTreeMap<_, _> = snapshot
        .nodes
        .iter()
        .filter_map(|(id, record)| match record {
            NodeRecord::Definition { fqn, .. } => Some((*id, fqn.clone())),
            NodeRecord::Package { import_path, .. } => Some((*id, import_path.clone())),
            NodeRecord::External { .. } => None,
        })
        .collect();
    let rows = snapshot
        .rows
        .iter()
        .map(|(key, record)| {
            let outcome = match &record.outcome {
                StoredOutcome::Resolved(id) => format!(
                    "resolved {}",
                    names.get(id).map_or("<unnamed node>", String::as_str)
                ),
                StoredOutcome::External(package) => format!("external {package}"),
                StoredOutcome::Unresolved(code) => reason_name(*code).to_string(),
            };
            (key.raw_target.clone(), key.kind, outcome)
        })
        .collect();
    Scan { rows }
}

impl Scan {
    /// The outcome of the one type use written as `raw_target`.
    #[track_caller]
    fn type_use(&self, raw_target: &str) -> &str {
        self.one(raw_target, RefKind::TypeUse)
    }

    #[track_caller]
    fn one(&self, raw_target: &str, kind: RefKind) -> &str {
        let code = kind.code();
        let hits: Vec<&str> = self
            .rows
            .iter()
            .filter(|(raw, k, _)| raw == raw_target && *k == code)
            .map(|(_, _, outcome)| outcome.as_str())
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one `{raw_target}` {kind:?} row, found {}\n{}",
            hits.len(),
            self.dump(),
        );
        hits[0]
    }

    /// How many rows of this kind carry this site text. Zero is the assertion
    /// a declaration's own name makes.
    fn count(&self, raw_target: &str, kind: RefKind) -> usize {
        let code = kind.code();
        self.rows
            .iter()
            .filter(|(raw, k, _)| raw == raw_target && *k == code)
            .count()
    }

    fn dump(&self) -> String {
        let mut out = String::from("rows:\n");
        for (raw, kind, outcome) in &self.rows {
            out.push_str(&format!("  kind={kind} {raw:?} -> {outcome}\n"));
        }
        out
    }
}

const GO_MOD: &str = concat!(
    "module example.com/app\n\n",
    "go 1.22\n\n",
    "require github.com/pkg/errors v0.9.1\n",
);

const UTIL: &str = "package util\n\ntype Thing struct{}\n\ntype Other struct{}\n";

/// Every position the Go grammar writes a type name in, one type per position
/// so that each assertion names exactly one site.
#[test]
fn go_emits_a_type_use_for_every_written_type_position() {
    let scan = render(&[
        ("go.mod", GO_MOD),
        ("util/util.go", UTIL),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "import (\n",
                "\t\"fmt\"\n",
                "\t\"example.com/app/util\"\n",
                "\t\"github.com/pkg/errors\"\n",
                ")\n\n",
                "type Param struct{}\n",
                "type Result struct{}\n",
                "type FieldT struct{}\n",
                "type Embedded struct{}\n",
                "type Recv struct{}\n",
                "type Lit struct{}\n",
                "type Declared struct{}\n",
                "type Asserted struct{}\n",
                "type Cased struct{}\n",
                "type Aliased struct{}\n",
                "type Elem struct{}\n",
                "type Iface interface{ Do() }\n\n",
                "type Holder struct {\n",
                "\tf FieldT\n",
                "\tEmbedded\n",
                "}\n\n",
                "type Names = Aliased\n\n",
                "type List []Elem\n\n",
                "type Composite interface {\n",
                "\tfmt.Stringer\n",
                "\tIface\n",
                "}\n\n",
                "func (r *Recv) M() {}\n\n",
                "func Sig(p Param) *Result { return nil }\n\n",
                "func Uses(x interface{}) {\n",
                "\tvar d Declared\n",
                "\t_ = d\n",
                "\t_ = Lit{}\n",
                "\t_ = util.Other{}\n",
                "\tif a, ok := x.(*Asserted); ok {\n",
                "\t\t_ = a\n",
                "\t}\n",
                "\tswitch c := x.(type) {\n",
                "\tcase *Cased:\n",
                "\t\t_ = c\n",
                "\t}\n",
                "\t_ = []byte(\"s\")\n",
                "\tvar e errors.Frame\n",
                "\t_ = e\n",
                "\tvar u util.Thing\n",
                "\t_ = u\n",
                "}\n",
            ),
        ),
    ]);

    // Signature positions.
    assert_eq!(scan.type_use("Param"), "resolved example.com/app#Param");
    assert_eq!(scan.type_use("Result"), "resolved example.com/app#Result");
    // A method receiver names its own type, and that is a written type name
    // like any other — the same choice Java and TypeScript already make for a
    // signature's types.
    assert_eq!(scan.type_use("Recv"), "resolved example.com/app#Recv");

    // Declaration positions.
    assert_eq!(scan.type_use("FieldT"), "resolved example.com/app#FieldT");
    assert_eq!(
        scan.type_use("Embedded"),
        "resolved example.com/app#Embedded",
        "an embedded struct field is a type use and nothing else",
    );
    assert_eq!(
        scan.type_use("Declared"),
        "resolved example.com/app#Declared"
    );
    assert_eq!(
        scan.type_use("Aliased"),
        "resolved example.com/app#Aliased",
        "`type Names = Aliased` names `Aliased`",
    );
    assert_eq!(scan.type_use("Elem"), "resolved example.com/app#Elem");
    assert_eq!(
        scan.type_use("Iface"),
        "resolved example.com/app#Iface",
        "an embedded interface is a type use",
    );

    // Expression positions.
    assert_eq!(
        scan.type_use("Lit"),
        "resolved example.com/app#Lit",
        "a composite literal's type — the largest single position in the corpus",
    );
    assert_eq!(
        scan.type_use("Asserted"),
        "resolved example.com/app#Asserted"
    );
    assert_eq!(scan.type_use("Cased"), "resolved example.com/app#Cased");

    // Qualified names, in and out of the module.
    assert_eq!(
        scan.type_use("util.Thing"),
        "resolved example.com/app/util#Thing",
    );
    assert_eq!(
        scan.type_use("util.Other"),
        "resolved example.com/app/util#Other",
        "a qualified composite literal is the same two-segment lookup as a call",
    );
    assert_eq!(
        scan.type_use("fmt.Stringer"),
        "external fmt",
        "a standard-library type is a link out of the repository",
    );
    assert_eq!(
        scan.type_use("errors.Frame"),
        "external github.com/pkg/errors",
    );

    // A conversion written with a composite type is a type use of the element.
    assert_eq!(
        scan.type_use("byte"),
        "external go:builtin",
        "`[]byte(s)` names the predeclared `byte`",
    );

    // A type declaration's own name is not a reference to it.
    assert_eq!(
        scan.count("Holder", RefKind::TypeUse),
        0,
        "a `type_spec` name is a declaration, not a use",
    );
    assert_eq!(
        scan.count("Names", RefKind::TypeUse),
        0,
        "a type alias's own name is a declaration, not a use",
    );
    assert_eq!(scan.count("List", RefKind::TypeUse), 0);
    assert_eq!(scan.count("Composite", RefKind::TypeUse), 0);
}

/// Go's universe scope holds types as well as functions, and it is the
/// outermost scope — so a package that declares its own `int` shadows the
/// predeclared one and the builtin answer is only reached after every
/// in-scope candidate has missed.
#[test]
fn go_predeclared_type_names_are_builtins_unless_the_package_declares_one() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type rune struct{}\n\n",
                "func F(a int, b string, c error, d rune) any { return nil }\n",
            ),
        ),
    ]);

    assert_eq!(scan.type_use("int"), "external go:builtin");
    assert_eq!(scan.type_use("string"), "external go:builtin");
    assert_eq!(scan.type_use("error"), "external go:builtin");
    assert_eq!(scan.type_use("any"), "external go:builtin");
    // The package declares `rune`, so the universe never answers for it.
    assert_eq!(
        scan.type_use("rune"),
        "resolved example.com/app#rune",
        "a package-level declaration shadows the universe scope",
    );
}

/// A type parameter is bound by the signature that declares it and is
/// nameable nowhere else, so it is a local binding — the same answer Java,
/// TypeScript and JavaScript already give for theirs.
#[test]
fn go_type_parameters_are_local_bindings() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Real struct{}\n\n",
                "func Generic[G any](g G) Real { return Real{} }\n",
            ),
        ),
    ]);

    assert_eq!(
        scan.type_use("G"),
        "LocalBinding",
        "a type parameter names nothing outside its signature",
    );
    assert_eq!(scan.type_use("any"), "external go:builtin");
    // The companion that makes the assertion above mean something: a real
    // type in the same signature still links.
    assert_eq!(scan.type_use("Real"), "resolved example.com/app#Real");
}

/// A generic type's parameter list binds inside the type's own body, at
/// package level, where there is no enclosing function at all.
#[test]
fn go_a_generic_type_declaration_binds_its_parameters_in_its_body() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Payload struct{}\n\n",
                "type Box[B any] struct {\n",
                "\tv B\n",
                "\tp Payload\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(scan.type_use("B"), "LocalBinding");
    assert_eq!(scan.type_use("Payload"), "resolved example.com/app#Payload");
}

/// A method on a generic type re-declares the type's parameters in its
/// receiver: `func (b *Box[R]) Get() R` *binds* `R` and does not name one.
#[test]
fn go_a_generic_receivers_type_arguments_bind_rather_than_reference() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Box[B any] struct{ v B }\n\n",
                "func (b *Box[R]) Get() R { return b.v }\n",
            ),
        ),
    ]);

    assert_eq!(
        scan.count("R", RefKind::TypeUse),
        1,
        "the receiver's `R` declares; only the result's `R` is a reference",
    );
    assert_eq!(scan.type_use("R"), "LocalBinding");
    assert_eq!(
        scan.type_use("Box"),
        "resolved example.com/app#Box",
        "the receiver's own type is still a type use",
    );
}

/// A `type` declared inside a function body is nameable nowhere else, so it
/// is a local binding — and its scope begins at the identifier, not at the
/// end of the declaration, so a recursive local type sees itself.
#[test]
fn go_function_local_types_are_local_bindings() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Outer struct{}\n\n",
                "func Local() {\n",
                "\ttype ring struct{ next *ring }\n",
                "\tvar r ring\n",
                "\tvar o Outer\n",
                "\t_ = r\n",
                "\t_ = o\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(
        scan.type_use("ring"),
        "LocalBinding",
        "the recursive field and the `var` are the same local type",
    );
    assert_eq!(
        scan.type_use("Outer"),
        "resolved example.com/app#Outer",
        "a package-level type used inside a function is still a node",
    );
}

/// A conversion written as a bare name (`T(x)`) is a `call_expression` in
/// Go's grammar and was already a `Call`. Emitting a type use for it too
/// would count one site twice.
#[test]
fn go_does_not_double_emit_a_bare_conversion() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Celsius float64\n\n",
                "func F(x int) Celsius { return Celsius(x) }\n",
            ),
        ),
    ]);

    // Two sites, two rows: the result type and the conversion. The conversion
    // is a call, not a type use.
    assert_eq!(scan.count("Celsius", RefKind::TypeUse), 1);
    assert_eq!(scan.count("Celsius", RefKind::Call), 1);
    assert_eq!(scan.type_use("Celsius"), "resolved example.com/app#Celsius");
    assert_eq!(
        scan.one("Celsius", RefKind::Call),
        "resolved example.com/app#Celsius",
    );
    // The right-hand side of a type declaration is a use like any other.
    assert_eq!(scan.type_use("float64"), "external go:builtin");
    assert_eq!(scan.type_use("int"), "external go:builtin");
}

/// A package-level `type A = B` declares `A`. Nothing referenced an alias
/// before type uses existed, so the extractor's `def-type` rule reading only
/// `type_spec` — and never `type_alias` — cost nothing and showed up nowhere.
///
/// Emitting type uses turned that into 57 `NoMatchingDefinition` rows on the
/// Go corpus, which is the bucket this project reserves for *its own* bugs.
/// The name really is in the repository; the extractor simply never wrote it
/// down.
#[test]
fn go_a_package_level_type_alias_is_a_node_and_its_uses_reach_it() {
    let scan = render(&[
        ("go.mod", GO_MOD),
        ("util/util.go", UTIL),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "import \"example.com/app/util\"\n\n",
                "type Concrete struct{}\n\n",
                "type Alias = Concrete\n\n",
                "type Foreign = util.Thing\n\n",
                "func Use(a Alias, f Foreign) {}\n",
            ),
        ),
    ]);

    assert_eq!(scan.type_use("Alias"), "resolved example.com/app#Alias");
    assert_eq!(scan.type_use("Foreign"), "resolved example.com/app#Foreign");
    // The alias's right-hand side is a use in its own right and reaches the
    // thing being aliased, in this package and across one.
    assert_eq!(
        scan.type_use("Concrete"),
        "resolved example.com/app#Concrete"
    );
    assert_eq!(
        scan.type_use("util.Thing"),
        "resolved example.com/app/util#Thing",
    );
}

/// §TypeSwitchCase: `case nil:` is legal and names the predeclared `nil`.
///
/// The grammar writes it as a `type_identifier` like any other, so the
/// extractor emits it — and the resolver has to answer for it. `nil` sits in
/// Go's universe block beside `int` and `error`, so it answers the same way,
/// and for the same reason: the name is real, it is outside this repository,
/// and `NoMatchingDefinition` would blame the corpus for a name arthron
/// simply did not know.
#[test]
fn go_a_nil_type_switch_case_names_the_predeclared_nil() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Real struct{}\n\n",
                "func F(v interface{}) {\n",
                "\tswitch v.(type) {\n",
                "\tcase nil:\n",
                "\tcase *Real:\n",
                "\t}\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(scan.type_use("nil"), "external go:builtin");
    assert_eq!(scan.type_use("Real"), "resolved example.com/app#Real");
}
