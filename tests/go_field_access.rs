//! Go's remaining reference sites: a selector read that is not a call, and a
//! struct literal's field keys.
//!
//! Before this file the Go extractor emitted a reference for a *call* through
//! a selector and for a written type name, and nothing for the two other
//! places the grammar names a member. `pkg.Name` read as a value produced no
//! row at all; neither did `t.field`, `x.y` or the `Field` in `T{Field: v}`.
//! Every one of those is a site in one file naming something outside it, which
//! is the whole definition of a reference in this project — so their absence
//! was a denominator that stopped short, not a language without the shape.
//!
//! Both constructs are [`RefKind::FieldAccess`]: "a field read or write" is
//! what `x.y` is, and a struct literal's key is the write half of the same
//! thing. Go writes a *method value* (`t.M` passed as a func) with the same
//! syntax as a field read and the grammar does not separate them, so one kind
//! answers for both — unlike Java, where `::` makes `MethodRef` a distinct
//! site.
//!
//! What each shape may honestly answer:
//!
//! - `Resolved` — `pkg.Name` through an import in this module, and a member
//!   the owning *type* declares: a method value reached through the receiver
//!   (`t.M`) or through the type itself (`T.M`).
//! - `External` — `pkg.Name` where the package is a dependency.
//! - `LocalBinding` — the selector's root is a parameter, a local, or a
//!   function-local type.
//! - `NeedsReceiverType` — the owner is a type in this repository and the
//!   member is not indexed. Go struct *fields* are not nodes in this build, so
//!   this is where an honest field read lands, exactly as a receiver-rooted
//!   call already did.
//! - `NeedsTypeInference` — the qualifier names something whose type this file
//!   does not state.
//! - `NeedsExpressionType` — the operand is an expression: `f().x`, `m[k].x`.
//!
//! No new [`arthron::UnresolvedReason`] was needed, which is the test that the
//! ratified taxonomy already covered this surface.

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
    /// The outcome of the one field access written as `raw_target`.
    #[track_caller]
    fn field(&self, raw_target: &str) -> &str {
        self.one(raw_target, RefKind::FieldAccess)
    }

    /// The outcome of the one type use written as `raw_target`.
    #[track_caller]
    fn type_use_of(&self, raw_target: &str) -> &str {
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

    /// How many rows of this kind carry this site text.
    fn count(&self, raw_target: &str, kind: RefKind) -> usize {
        let code = kind.code();
        self.rows
            .iter()
            .filter(|(raw, k, _)| raw == raw_target && *k == code)
            .count()
    }

    /// Every field-access row, for a whole-file assertion.
    fn field_targets(&self) -> Vec<&str> {
        let code = RefKind::FieldAccess.code();
        let mut out: Vec<&str> = self
            .rows
            .iter()
            .filter(|(_, k, _)| *k == code)
            .map(|(raw, _, _)| raw.as_str())
            .collect();
        out.sort_unstable();
        out
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

const UTIL: &str = concat!(
    "package util\n\n",
    "type Thing struct{ Name string }\n\n",
    "var Global = 1\n\n",
    "func Helper() {}\n",
);

/// A value read through an import is the same two-segment lookup a call is,
/// and it reaches the same node.
#[test]
fn go_a_package_qualified_read_resolves_through_the_import() {
    let scan = render(&[
        ("go.mod", GO_MOD),
        ("util/util.go", UTIL),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "import (\n",
                "\t\"os\"\n",
                "\t\"example.com/app/util\"\n",
                ")\n\n",
                "func Read() {\n",
                "\t_ = util.Global\n",
                "\t_ = util.Helper\n",
                "\t_ = os.Args\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(
        scan.field("util.Global"),
        "resolved example.com/app/util#Global",
        "a package-qualified value read is a reference to that package's node",
    );
    assert_eq!(
        scan.field("util.Helper"),
        "resolved example.com/app/util#Helper",
        "a function named rather than called still names the function",
    );
    assert_eq!(scan.field("os.Args"), "external os");
    // The read is not also a call: nothing was invoked.
    assert_eq!(scan.count("util.Helper", RefKind::Call), 0);
}

/// A member selected through the method's own receiver is `this`, for a read
/// exactly as for a call — the shape PR #48 re-rooted.
#[test]
fn go_a_receiver_rooted_read_is_this_and_not_a_local() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Conn struct{ name string }\n\n",
                "func (c *Conn) Name() string { return c.name }\n\n",
                "func (c *Conn) Pass() func() string {\n",
                "\treturn c.Name\n",
                "}\n",
            ),
        ),
    ]);

    // A method value reached through the receiver: the receiver's type is
    // written in the signature, so this is a declared-type lookup and it hits.
    assert_eq!(
        scan.field("c.Name"),
        "resolved example.com/app#Conn.Name",
        "a method value through the receiver reaches the method",
    );
    // A struct field is not a node in this build, and the receiver type is
    // stated — so the honest answer is that the member is not indexed, not
    // that the repository is missing a name.
    assert_eq!(
        scan.field("c.name"),
        "NeedsReceiverType",
        "Go struct fields are not indexed; the receiver type is known",
    );
}

/// `T.M` names a method through the type itself — Go's method expression.
#[test]
fn go_a_method_expression_reaches_the_method_it_names() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Conn struct{}\n\n",
                "var Global Conn\n\n",
                "func (c Conn) Ping() {}\n\n",
                "func Pass() {\n",
                "\t_ = Conn.Ping\n",
                "\t_ = Global.Ping\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(
        scan.field("Conn.Ping"),
        "resolved example.com/app#Conn.Ping",
        "a method expression names the method on the type",
    );
    // A package-level *variable* is not a type, and its type is not stated at
    // the site — so this one honestly needs inference rather than claiming the
    // owner is a type whose members are unindexed.
    assert_eq!(scan.field("Global.Ping"), "NeedsTypeInference");
}

/// The root-binding rule reaches a read exactly as it reaches a call: depth is
/// irrelevant, and a parameter's field is outside both terms of the rate.
#[test]
fn go_a_read_rooted_at_a_local_is_a_local_binding() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Conn struct{ name string }\n\n",
                "func Use(c Conn) string {\n",
                "\tlocal := c\n",
                "\t_ = local.name\n",
                "\treturn c.name\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(scan.field("c.name"), "LocalBinding");
    assert_eq!(scan.field("local.name"), "LocalBinding");
}

/// A selector whose operand is an expression names no binding at all, so the
/// reason says what is actually missing.
#[test]
fn go_a_read_on_an_expression_needs_the_expressions_type() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Conn struct{ name string }\n\n",
                "func make2() Conn { return Conn{} }\n\n",
                "func Use(m map[string]Conn) {\n",
                "\t_ = make2().name\n",
                "\t_ = m[\"k\"].name\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(scan.field("make2().name"), "NeedsExpressionType");
    assert_eq!(scan.field("m[\"k\"].name"), "NeedsExpressionType");
}

/// One site, one row: a selector standing in the callee position of a call is
/// the call's own reference and is not read a second time.
#[test]
fn go_a_call_through_a_selector_is_not_also_a_field_access() {
    let scan = render(&[
        ("go.mod", GO_MOD),
        ("util/util.go", UTIL),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "import \"example.com/app/util\"\n\n",
                "type Conn struct{ inner Conn2 }\n",
                "type Conn2 struct{}\n\n",
                "func (c Conn2) Ping() {}\n\n",
                "func Use(c Conn) {\n",
                "\tutil.Helper()\n",
                "\tc.inner.Ping()\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(
        scan.one("util.Helper", RefKind::Call),
        "resolved example.com/app/util#Helper",
    );
    assert_eq!(scan.count("util.Helper", RefKind::FieldAccess), 0);
    // The intermediate selector of a call chain belongs to the call: one row
    // for `c.inner.Ping()`, none for the `c.inner` inside it.
    assert_eq!(scan.count("c.inner.Ping", RefKind::Call), 1);
    assert_eq!(scan.count("c.inner", RefKind::FieldAccess), 0);
    assert_eq!(scan.count("c.inner.Ping", RefKind::FieldAccess), 0);
}

/// A struct literal states its own type at the site, so its keys are looked up
/// on that type rather than inferred.
#[test]
fn go_a_struct_literal_key_names_a_member_of_the_stated_type() {
    let scan = render(&[
        ("go.mod", GO_MOD),
        ("util/util.go", UTIL),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "import (\n",
                "\t\"example.com/app/util\"\n",
                "\t\"github.com/pkg/errors\"\n",
                ")\n\n",
                "type Conn struct{ name string }\n\n",
                "func Build() {\n",
                "\t_ = Conn{name: \"a\"}\n",
                "\t_ = util.Thing{Name: \"b\"}\n",
                "\t_ = errors.Frame{Line: 1}\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(
        scan.field("Conn.name"),
        "NeedsReceiverType",
        "the literal's type is stated and in this repository; the field is not indexed",
    );
    assert_eq!(scan.field("util.Thing.Name"), "NeedsReceiverType");
    assert_eq!(
        scan.field("errors.Frame.Line"),
        "external github.com/pkg/errors",
        "a key on a dependency's type is a link out of the repository",
    );
}

/// An elided nested literal states its type once, on the container.
#[test]
fn go_an_elided_nested_literal_key_names_the_element_type() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Conn struct{ name string }\n\n",
                "func Build() {\n",
                "\t_ = []Conn{{name: \"a\"}}\n",
                "\t_ = []*Conn{{name: \"b\"}}\n",
                "\t_ = map[string]Conn{\"k\": {name: \"c\"}}\n",
                "}\n",
            ),
        ),
    ]);

    // Three sites, one row: same file, same encloser, same target.
    assert_eq!(scan.field("Conn.name"), "NeedsReceiverType");
    assert_eq!(scan.field_targets(), ["Conn.name"]);
}

/// A map literal's key is an expression that is evaluated, not a member name.
/// The Go extractor emits no reference for a bare identifier read, so it emits
/// none here either.
#[test]
fn go_a_map_literal_key_is_an_expression_and_not_a_reference() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "const Key = \"k\"\n\n",
                "func Build() {\n",
                "\t_ = map[string]int{Key: 1}\n",
                "\t_ = [...]int{0: 1, 1: 2}\n",
                "}\n",
            ),
        ),
    ]);

    assert!(
        scan.field_targets().is_empty(),
        "a map or array key is an expression: {}",
        scan.dump(),
    );
}

/// An anonymous struct type has no canonical name, so it is not a node and
/// neither are its fields: there is nothing for a key to name.
#[test]
fn go_an_anonymous_struct_literal_key_names_nothing() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "func Build() {\n",
                "\t_ = []struct{ name string }{{name: \"a\"}}\n",
                "}\n",
            ),
        ),
    ]);

    assert!(
        scan.field_targets().is_empty(),
        "an anonymous struct type is not a node: {}",
        scan.dump(),
    );
}

/// A `type` declared inside a function body is nameable nowhere else, so a
/// literal of it names nothing outside the block — the same answer its type
/// uses already give.
#[test]
fn go_a_function_local_types_literal_key_is_a_local_binding() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "func Build() {\n",
                "\ttype ring struct{ next int }\n",
                "\t_ = ring{next: 1}\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(scan.field("ring.next"), "LocalBinding");
}

/// `string(b)` is a conversion, and Go writes it exactly as it writes a call.
///
/// The grammar files it as a `call_expression`, so it arrived at the resolver
/// as a `Call`, missed the list of predeclared *functions*, and was reported
/// `NoMatchingDefinition` — the bucket this project reserves for its own bugs,
/// whose contract is that the lookup table was complete and the name absent.
/// The name is not absent: it is in Go's universe block. It was 123 rows on
/// the `codeiq` corpus and 269 on `caddy`, and every one of them a predeclared
/// type name.
#[test]
fn go_a_one_argument_conversion_to_a_predeclared_type_is_a_builtin() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type Celsius float64\n\n",
                "func F(b []byte, n int) {\n",
                "\t_ = string(b)\n",
                "\t_ = int64(n)\n",
                "\t_ = Celsius(n)\n",
                "\t_ = len(b)\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(scan.one("string", RefKind::Call), "external go:builtin");
    assert_eq!(scan.one("int64", RefKind::Call), "external go:builtin");
    // The two neighbours that must not move: a repository conversion still
    // resolves, and a predeclared *function* answers from the function list.
    assert_eq!(
        scan.one("Celsius", RefKind::Call),
        "resolved example.com/app#Celsius",
    );
    assert_eq!(scan.one("len", RefKind::Call), "external go:builtin");
}

/// The universe is the outermost scope, so a package that declares the name
/// itself still wins — for a conversion exactly as for a type use.
#[test]
fn go_a_package_declared_name_still_shadows_the_universe_at_a_conversion() {
    let scan = render(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "app.go",
            concat!(
                "package app\n\n",
                "type rune struct{}\n\n",
                "func F(v interface{}) {\n",
                "\t_ = rune{}\n",
                "\t_ = string(nil)\n",
                "}\n",
            ),
        ),
    ]);

    assert_eq!(scan.type_use_of("rune"), "resolved example.com/app#rune");
    assert_eq!(scan.one("string", RefKind::Call), "external go:builtin");
}
