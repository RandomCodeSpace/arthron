//! Resolver-level acceptance for Java: what each shape in a small synthetic
//! tree actually resolves to.
//!
//! `tests/java_extract.rs` checks the records one file yields and
//! `tests/java_corpus.rs` checks the aggregate rate on a real corpus. Neither
//! can say *which definition* a site linked to, and a wrong edge changes
//! neither of their numbers — a call linked to the wrong type's method is
//! still one `resolved`. So this file resolves a whole tree and asserts the
//! target by name.
//!
//! Every case below was a defect first: the assertion is the shape the
//! adversarial review on PR #9 measured coming out wrong.

use std::collections::BTreeMap;

use arthron::model::RefKind;
use arthron::store::{NodeRecord, Store, StoredOutcome};
use arthron::track_java::scan_java;

/// Every reference row of one scan, rendered as a readable outcome.
struct Scan {
    /// `(file, raw_target, kind code)` → outcome text, in row order.
    rows: Vec<Row>,
}

struct Row {
    file: String,
    raw_target: String,
    kind: u8,
    enclosing: String,
    outcome: String,
}

/// Write `files` to a scratch tree, scan it, and render every row's outcome.
///
/// The rendering is deliberately the *name* a `Resolved` row points at and
/// not its `NodeId`: an id proves an edge exists, and what these tests are
/// about is whether it points at the right definition.
fn scan(files: &[(&str, &str)]) -> Scan {
    let dir = tempfile::tempdir().expect("a scratch directory");
    for (path, source) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().expect("a file has a parent"))
            .expect("creating the package directory");
        std::fs::write(&full, source).expect("writing a source file");
    }
    let db = dir.path().join("graph.redb");
    // The report is not asserted to be non-empty: a file whose every
    // construct sits inside a parse error contributes no references, and
    // that is the correct answer rather than a broken scan.
    scan_java(dir.path(), &db).expect("scan");
    let store = Store::open(&db).expect("the store opens");
    let snapshot = store.snapshot().expect("snapshot");
    let names: BTreeMap<_, _> = snapshot
        .nodes
        .iter()
        .filter_map(|(id, record)| match record {
            NodeRecord::Definition { fqn, .. } => Some((*id, fqn.clone())),
            _ => None,
        })
        .collect();
    let rows = snapshot
        .rows
        .iter()
        .map(|(key, record)| Row {
            file: key.file.clone(),
            raw_target: key.raw_target.clone(),
            kind: key.kind,
            enclosing: key.enclosing.clone(),
            outcome: match &record.outcome {
                StoredOutcome::Resolved(id) => format!(
                    "RESOLVED {}",
                    names.get(id).map_or("<unnamed node>", String::as_str)
                ),
                StoredOutcome::External(package) => format!("EXTERNAL {package}"),
                StoredOutcome::Unresolved(code) => arthron::model::reason_name(*code).to_string(),
            },
        })
        .collect();
    Scan { rows }
}

impl Scan {
    /// The outcome of the one row with this site text and kind.
    ///
    /// Panics when there is not exactly one: a test that silently read the
    /// first of two rows would assert about whichever the row order happened
    /// to put first.
    #[track_caller]
    fn one(&self, raw_target: &str, kind: RefKind) -> &str {
        let code = kind.code();
        let mut hits = self
            .rows
            .iter()
            .filter(|r| r.raw_target == raw_target && r.kind == code);
        let first = hits.next().unwrap_or_else(|| {
            panic!("no `{raw_target}` {kind:?} row\n{}", self.dump());
        });
        assert!(
            hits.next().is_none(),
            "`{raw_target}` {kind:?} has more than one row\n{}",
            self.dump(),
        );
        &first.outcome
    }

    /// Every row, for a failure message that says what was actually measured.
    fn dump(&self) -> String {
        let mut out = String::from("rows:\n");
        for r in &self.rows {
            out.push_str(&format!(
                "  {} kind={} in {} :: {:?} -> {}\n",
                r.file, r.kind, r.enclosing, r.raw_target, r.outcome
            ));
        }
        out
    }
}

/// The tree the anonymous-class cases are measured in.
///
/// `Anon extends Other`, the anonymous body extends `Base`, and both `Base`
/// and `Other` declare `shared()` and a `name` field — so every wrong answer
/// is a *different named definition* rather than a miss, which is what makes
/// the assertions able to tell a wrong edge from a lowered rate.
fn anonymous_class_tree() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "com/acme/Base.java",
            r#"package com.acme;
public class Base {
    String name = "base";
    public String shared() { return "base"; }
    public String onlyOnBase() { return "base"; }
}
"#,
        ),
        (
            "com/acme/Other.java",
            r#"package com.acme;
public class Other {
    String name = "other";
    public String shared() { return "other"; }
}
"#,
        ),
        (
            "com/acme/Anon.java",
            r#"package com.acme;
public class Anon extends Other {
    String name = "outer";
    public String shared() { return "outer"; }
    public Object make() {
        return new Base() {
            String name = "inner";
            public String shared() { return "inner"; }
            public String tag() {
                String a = super.shared();
                shared();
                onlyOnBase();
                return this.name;
            }
        };
    }
}
"#,
        ),
    ]
}

#[test]
fn super_inside_an_anonymous_class_targets_the_anonymous_supertype() {
    let scan = scan(&anonymous_class_tree());
    // `super` inside `new Base(){…}` is `Base`, never `Anon`'s own `extends
    // Other` (§15.11.2 reads the *immediately* enclosing class's superclass,
    // and that class is the anonymous one).
    assert_eq!(
        scan.one("super.shared", RefKind::Call),
        "RESOLVED com.acme#Base.shared/0",
    );
}

#[test]
fn an_unqualified_call_declared_by_an_anonymous_class_is_not_the_outer_types() {
    let scan = scan(&anonymous_class_tree());
    // §15.12.1 searches the innermost enclosing *type declaration* that has a
    // member of that name, and that is the anonymous class, which declares
    // `shared()`. The anonymous class is not a node (T-04), so there is no
    // honest edge — and `com.acme#Anon.shared/0` is a wrong one.
    assert_eq!(scan.one("shared", RefKind::Call), "LocalBinding");
}

#[test]
fn an_unqualified_call_inherited_by_an_anonymous_class_targets_the_supertype() {
    let scan = scan(&anonymous_class_tree());
    // The anonymous class does not declare `onlyOnBase`, so the search moves
    // to its supertype closure — where `Base` declares it. A real edge, and
    // one the erased frame was hiding.
    assert_eq!(
        scan.one("onlyOnBase", RefKind::Call),
        "RESOLVED com.acme#Base.onlyOnBase/0",
    );
}

#[test]
fn this_inside_an_anonymous_class_is_not_the_outer_types_field() {
    let scan = scan(&anonymous_class_tree());
    // `this` inside an anonymous class denotes the anonymous instance and
    // never the enclosing one (§15.8.3); `Anon#name` is a different field of
    // a different type.
    assert_eq!(scan.one("this.name", RefKind::FieldAccess), "LocalBinding");
}

/// N-03 tier 1 includes *inherited* member types (§8.5).
#[test]
fn a_member_type_inherited_from_a_supertype_resolves() {
    let scan = scan(&[
        (
            "com/acme/Holder.java",
            r#"package com.acme;
public class Holder {
    public enum State { OPEN }
    public static class Nested { }
}
"#,
        ),
        (
            "com/acme/Sub.java",
            r#"package com.acme;
public class Sub extends Holder {
    State s;
    Object o = State.OPEN;
    Nested n;
    Object x = new Nested();
}
"#,
        ),
    ]);
    assert_eq!(
        scan.one("State", RefKind::TypeUse),
        "RESOLVED com.acme#Holder$State"
    );
    assert_eq!(
        scan.one("State.OPEN", RefKind::FieldAccess),
        "RESOLVED com.acme#Holder$State.OPEN",
    );
    assert_eq!(
        scan.one("Nested", RefKind::TypeUse),
        "RESOLVED com.acme#Holder$Nested",
    );
    assert_eq!(
        scan.one("Nested", RefKind::New),
        "RESOLVED com.acme#Holder$Nested.<init>/0",
    );
}

/// X-07: a receiver whose declared type is a type variable resolves against
/// the variable's bound, and an unbounded one erases to `Object` (§4.6).
#[test]
fn a_receiver_typed_by_a_type_variable_resolves_against_its_bound() {
    let scan = scan(&[
        (
            "com/acme/Bound.java",
            r#"package com.acme;
public class Bound {
    public String tag() { return "b"; }
}
"#,
        ),
        (
            "com/acme/Generic.java",
            r#"package com.acme;
public class Generic<T extends Bound, U> {
    T value;
    U free;
    void m(T t) {
        t.tag();
        value.tag();
    }
    void n(U u) {
        u.hashCode();
        free.hashCode();
    }
}
"#,
        ),
    ]);
    assert_eq!(
        scan.one("t.tag", RefKind::Call),
        "RESOLVED com.acme#Bound.tag/0"
    );
    assert_eq!(
        scan.one("value.tag", RefKind::Call),
        "RESOLVED com.acme#Bound.tag/0",
    );
    // §4.6: an unbounded type variable erases to `Object`, whose members are
    // external and never a definition of this repository.
    assert_eq!(
        scan.one("u.hashCode", RefKind::Call),
        "EXTERNAL jdk:java.lang"
    );
    assert_eq!(
        scan.one("free.hashCode", RefKind::Call),
        "EXTERNAL jdk:java.lang",
    );
}

/// The tree the overload-set cases are measured in.
fn overload_tree() -> Vec<(&'static str, &'static str)> {
    vec![(
        "com/acme/Only.java",
        r#"package com.acme;
public class Only {
    public static final int CONST = 1;
    public static int only(int a) { return a; }
    public static int many(int a) { return a; }
    public static int many(String a) { return 1; }
}
"#,
    )]
}

/// C-08 / X-05: a method reference resolves when the overload set is a
/// singleton, and reports ambiguity only when there is one.
#[test]
fn a_method_reference_to_a_singleton_resolves() {
    let mut files = overload_tree();
    files.push((
        "com/acme/Refs.java",
        r#"package com.acme;
public class Refs {
    Object a = (java.util.function.IntUnaryOperator) Only::only;
    Object b = (java.util.function.IntUnaryOperator) Only::many;
    Object c = (java.util.function.IntUnaryOperator) Only::absent;
}
"#,
    ));
    let scan = scan(&files);
    assert_eq!(
        scan.one("Only::only", RefKind::MethodRef),
        "RESOLVED com.acme#Only.only/1",
    );
    assert_eq!(
        scan.one("Only::many", RefKind::MethodRef),
        "AmbiguousOverload"
    );
    // `Only` is declared in another file, so nothing here can read its
    // supertypes — `absent` may be inherited and this resolver cannot say.
    // That is `UnindexedSupertype`, and it is a *different* answer from the
    // overload set's, which is the whole point.
    assert_eq!(
        scan.one("Only::absent", RefKind::MethodRef),
        "UnindexedSupertype",
    );
}

/// I-04: a single-static import of a method is not `AmbiguousOverload` by
/// construction, and one naming nothing at all is distinguishable from one
/// naming five overloads.
#[test]
fn a_single_static_import_of_a_method_is_not_ambiguous_by_construction() {
    let mut files = overload_tree();
    files.push((
        "com/acme/use/Imports.java",
        r#"package com.acme.use;
import static com.acme.Only.CONST;
import static com.acme.Only.only;
import static com.acme.Only.many;
import static com.acme.Only.absent;
public class Imports { }
"#,
    ));
    let scan = scan(&files);
    assert_eq!(
        scan.one("com.acme.Only.CONST", RefKind::Import),
        "RESOLVED com.acme#Only.CONST",
    );
    assert_eq!(
        scan.one("com.acme.Only.only", RefKind::Import),
        "RESOLVED com.acme#Only.only/1",
    );
    assert_eq!(
        scan.one("com.acme.Only.many", RefKind::Import),
        "AmbiguousOverload",
    );
    assert_eq!(
        scan.one("com.acme.Only.absent", RefKind::Import),
        "UnindexedSupertype",
    );
}

/// N-04 / B-02: a fully qualified name is attributed to its package in value
/// position exactly as it is in type position.
#[test]
fn a_fully_qualified_call_is_external_like_the_same_name_in_type_position() {
    let scan = scan(&[(
        "com/acme/Q.java",
        r#"package com.acme;
public class Q {
    java.util.List<String> field;
    Object a = java.util.Objects.requireNonNull("x");
}
"#,
    )]);
    assert_eq!(
        scan.one("java.util.List", RefKind::TypeUse),
        "EXTERNAL jdk:java.util",
    );
    assert_eq!(
        scan.one("java.util.Objects.requireNonNull", RefKind::Call),
        "EXTERNAL jdk:java.util",
    );
}

/// B-01/B-02: absence inside a package this repository declares is a missing
/// definition, not a dependency.
#[test]
fn a_missing_type_in_a_declared_package_is_not_external() {
    let scan = scan(&[(
        "com/acme/Miss.java",
        r#"package com.acme;
public class Miss {
    com.acme.Nope field;
}
"#,
    )]);
    assert_eq!(
        scan.one("com.acme.Nope", RefKind::TypeUse),
        "NoMatchingDefinition",
    );
}

/// X-02: `this.f.m()` reads the same declared-type environment `f.m()` does.
#[test]
fn a_this_qualified_field_receiver_resolves_like_the_bare_one() {
    let scan = scan(&[
        (
            "com/acme/Bound.java",
            r#"package com.acme;
public class Bound {
    public String tag() { return "b"; }
}
"#,
        ),
        (
            "com/acme/Chain.java",
            r#"package com.acme;
public class Chain {
    Bound field = new Bound();
    void m() {
        field.tag();
        this.field.tag();
    }
}
"#,
        ),
    ]);
    assert_eq!(
        scan.one("field.tag", RefKind::Call),
        "RESOLVED com.acme#Bound.tag/0",
    );
    assert_eq!(
        scan.one("this.field.tag", RefKind::Call),
        "RESOLVED com.acme#Bound.tag/0",
    );
}

/// C-05: an anonymous class implementing an interface invokes
/// `Object#<init>()` (§15.9.5.1), never a constructor of the interface.
#[test]
fn an_anonymous_class_on_an_interface_creates_an_object() {
    let scan = scan(&[
        (
            "com/acme/Iface.java",
            r#"package com.acme;
public interface Iface {
    void run();
}
"#,
        ),
        (
            "com/acme/UseIface.java",
            r#"package com.acme;
public class UseIface {
    Object o = new Iface() {
        public void run() { }
    };
}
"#,
        ),
    ]);
    assert_eq!(scan.one("Iface", RefKind::New), "EXTERNAL jdk:java.lang");
}

/// A `type_identifier` tree-sitter recovered from inside an `ERROR` region is
/// not a reference to anything.
///
/// The bytes below are the 37-byte kernel of the fuzz-corpus literal in
/// `ClassUtilsOssFuzzTest.java`, shrunk from the 15,201-byte original by
/// bisection: they break the string literal, and recovery reads type names
/// out of the wreckage. In the corpus that produced one row carrying 405
/// occurrences of `External("$$")` — which the gate baseline was then
/// defending.
#[test]
fn a_type_identifier_inside_a_parse_error_is_not_a_reference() {
    let payload = "]\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0~\0\0\0\0\0\0\0\0\0\0t\0$t\0$";
    let source = format!("package com.acme;\npublic class Fuzz {{ String s = \"{payload}\"; }}\n");
    let scan = scan(&[("com/acme/Fuzz.java", source.as_str())]);
    assert!(
        scan.rows.is_empty(),
        "a reference was recovered from inside a parse error\n{}",
        scan.dump(),
    );
}
