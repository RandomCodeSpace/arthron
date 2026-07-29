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
    // The field form is what X-07 is measured on: a field is a node, so the
    // declared-type lookup still runs on it. The parameter beside it is the
    // same declared type read through a *local*, which the uniform
    // root-binding rule takes out of the rate — the type variable is resolved
    // identically either way, and only one of the two is a node.
    assert_eq!(
        scan.one("value.tag", RefKind::Call),
        "RESOLVED com.acme#Bound.tag/0",
    );
    assert_eq!(scan.one("t.tag", RefKind::Call), "LocalBinding");
    // §4.6: an unbounded type variable erases to `Object`, whose members are
    // external and never a definition of this repository.
    assert_eq!(
        scan.one("free.hashCode", RefKind::Call),
        "EXTERNAL jdk:java.lang",
    );
    assert_eq!(scan.one("u.hashCode", RefKind::Call), "LocalBinding");
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

/// §15.12.2: argument types written at the invocation site narrow a shared
/// arity key to the one applicable full-signature definition.
#[test]
fn written_argument_types_select_full_signature_overloads() {
    let scan = scan(&[(
        "com/acme/Arguments.java",
        r#"package com.acme;
public class Arguments {
    static class Built {
        Built(int value) {}
        Built(String value) {}
    }

    static void byInt(int value) {}
    static void byInt(String value) {}
    static void byString(int value) {}
    static void byString(String value) {}
    static void widen(long value) {}
    static void widen(String value) {}
    static void box(Integer value) {}
    static void box(String value) {}
    static void object(Object value) {}
    static void object(String value) {}
    static void casted(long value) {}
    static void casted(String value) {}
    static void created(Integer value) {}
    static void created(String value) {}
    static void unknown(int value) {}
    static void unknown(String value) {}
    static Object value() { return null; }

    static void calls() {
        Integer integer = 1;
        byInt(1);
        byString("text");
        widen(1);
        box(1);
        object(integer);
        casted((long) 1);
        created(new Integer(1));
        new Built(1);
        unknown(value());
    }
}
"#,
    )]);

    for (raw, target) in [
        ("byInt", "com.acme#Arguments.byInt(int)"),
        ("byString", "com.acme#Arguments.byString(String)"),
        ("widen", "com.acme#Arguments.widen(long)"),
        ("box", "com.acme#Arguments.box(Integer)"),
        ("object", "com.acme#Arguments.object(Object)"),
        ("casted", "com.acme#Arguments.casted(long)"),
        ("created", "com.acme#Arguments.created(Integer)"),
    ] {
        assert_eq!(
            scan.one(raw, RefKind::Call),
            format!("RESOLVED {target}"),
            "`{raw}` selected the wrong full-signature target",
        );
    }
    assert_eq!(
        scan.one("Built", RefKind::New),
        "RESOLVED com.acme#Arguments$Built.<init>(int)",
    );
    assert_eq!(
        scan.one("unknown", RefKind::Call),
        "AmbiguousOverload",
        "an argument whose type is not written must stay honest",
    );
}

/// §15.12.2.1-.5: every declaration at the invocation's arity participates
/// in applicability before phase order and specificity select a target.
#[test]
fn applicability_collects_direct_inherited_and_varargs_candidates() {
    let scan = scan(&[(
        "com/acme/Applicability.java",
        r#"package com.acme;
public class Applicability {
    static class Base {
        void inherited(int value) {}
        void inherited(long value) {}
    }

    static class Child extends Base {
        void inherited(String value) {}

        void callInherited() {
            inherited(1);
        }
    }

    static void flexible(String value) {}
    static void flexible(boolean value) {}
    static void flexible(int... value) {}
    static void flexible(long... value) {}

    static void phased(int value) {}
    static void phased(long value) {}
    static void phased(Integer... value) {}
    static void phased(Object... value) {}

    static void calls() {
        flexible(1);
        phased(1);
    }
}
"#,
    )]);

    assert_eq!(
        scan.one("inherited", RefKind::Call),
        "RESOLVED com.acme#Applicability$Base.inherited(int)",
        "an inapplicable direct declaration must not hide an inherited applicable overload",
    );
    assert_eq!(
        scan.one("flexible", RefKind::Call),
        "RESOLVED com.acme#Applicability.flexible(int...)",
        "an inapplicable fixed-arity set must not hide an applicable varargs set",
    );
    assert_eq!(
        scan.one("phased", RefKind::Call),
        "RESOLVED com.acme#Applicability.phased(int)",
        "an applicable fixed-arity method must beat every varargs candidate",
    );
}

/// JLS §5.1.2, §5.1.5, §5.1.7-.8 and §15.12.2.5: conversion depth is part
/// of most-specific selection; a supported but less-specific target cannot
/// win merely because it was the first signature probed.
#[test]
fn conversion_depths_choose_most_specific_primitive_and_wrapper_targets() {
    let scan = scan(&[(
        "com/acme/Conversions.java",
        r#"package com.acme;
public class Conversions {
    static void number(Number value) {}
    static void number(Object value) {}
    static void boxedReference(Number value) {}
    static void boxedReference(Object value) {}
    static void primitive(float value) {}
    static void primitive(double value) {}
    static void unboxed(long value) {}
    static void unboxed(double value) {}
    static void characterObject(Object value) {}
    static void characterObject(String value) {}
    static void incomparable(int left, long right) {}
    static void incomparable(long left, int right) {}
    static void unknownNull(Number value) {}
    static void unknownNull(Object value) {}
    static void unknownCall(Number value) {}
    static void unknownCall(Object value) {}
    static void unknownPoly(java.util.function.Consumer<String> value) {}
    static void unknownPoly(java.util.function.Function<String, String> value) {}
    static Integer value() { return null; }

    static void calls() {
        Integer integer = 1;
        Character character = 'x';
        unknownNull(null);
        unknownCall(value());
        unknownPoly(value -> value.trim());
        number(integer);
        boxedReference(1);
        primitive(1);
        unboxed(integer);
        characterObject(character);
        incomparable(1, 1);
    }
}
"#,
    )]);

    for raw in ["unknownNull", "unknownCall", "unknownPoly"] {
        assert_eq!(
            scan.one(raw, RefKind::Call),
            "AmbiguousOverload",
            "`{raw}` has no file-local standalone argument type and must stay honest",
        );
    }
    assert_eq!(
        scan.one("incomparable", RefKind::Call),
        "AmbiguousOverload",
        "incomparable conversion vectors must not be resolved by probe order",
    );
    for (raw, target) in [
        ("number", "com.acme#Conversions.number(Number)"),
        (
            "boxedReference",
            "com.acme#Conversions.boxedReference(Number)",
        ),
        ("primitive", "com.acme#Conversions.primitive(float)"),
        ("unboxed", "com.acme#Conversions.unboxed(long)"),
        (
            "characterObject",
            "com.acme#Conversions.characterObject(Object)",
        ),
    ] {
        assert_eq!(
            scan.one(raw, RefKind::Call),
            format!("RESOLVED {target}"),
            "`{raw}` selected the wrong full-signature target",
        );
    }
}

/// §15.15.3-.5: unary numeric operators apply unary numeric promotion, so a
/// literal operand still states a primitive argument type without inference.
#[test]
fn unary_numeric_literal_arguments_select_promoted_primitive_overloads() {
    let scan = scan(&[(
        "com/acme/UnaryArguments.java",
        r#"package com.acme;
public class UnaryArguments {
    static void minus(long value) {}
    static void minus(String value) {}
    static void plus(long value) {}
    static void plus(String value) {}
    static void complement(long value) {}
    static void complement(String value) {}

    static void calls() {
        minus(-1L);
        plus(+1L);
        complement(~1L);
    }
}
"#,
    )]);

    for raw in ["minus", "plus", "complement"] {
        assert_eq!(
            scan.one(raw, RefKind::Call),
            format!("RESOLVED com.acme#UnaryArguments.{raw}(long)"),
            "`{raw}` did not preserve the promoted literal type",
        );
    }
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

/// C-05, read off a facet instead of guessed at.
///
/// `Iface` is declared in another file, so the only thing that can say it is
/// an interface is the stored [`arthron::model::DefFacets`] — the guess this
/// replaces was "the constructor lookup missed, so it must have been an
/// interface", which is a statement about the *search* and not about the type.
#[test]
fn an_anonymous_creation_on_a_class_keeps_the_honest_miss() {
    let scan = scan(&[
        (
            "com/acme/Base.java",
            r#"package com.acme;
public class Base {
    public Base() { }
}
"#,
        ),
        (
            "com/acme/UseBase.java",
            r#"package com.acme;
public class UseBase {
    Object o = new Base(1) {
        public String tag() { return "t"; }
    };
}
"#,
        ),
    ]);
    // `Base` is a class and declares no one-argument constructor, so
    // §15.9.5.1 has nothing to say here: the site names a constructor that
    // the search could not finish looking for, which is not the same fact as
    // "the constructor invoked belongs to `java.lang.Object`". Externalising
    // it moved the reference out of both rate terms on the strength of a
    // guess.
    assert_eq!(scan.one("Base", RefKind::New), "UnindexedSupertype");
}

/// The same rule at the other end: a name nothing places is not an interface
/// either, and saying `java.lang` about it invented a package for it.
#[test]
fn an_anonymous_creation_on_an_unplaced_name_keeps_the_type_miss() {
    let scan = scan(&[(
        "com/acme/UseNowhere.java",
        r#"package com.acme;
public class UseNowhere {
    Object o = new Nowhere() {
        public String tag() { return "t"; }
    };
}
"#,
    )]);
    assert_eq!(scan.one("Nowhere", RefKind::New), "NoMatchingDefinition");
}

/// The bound on the rule: a creation whose constructor *is* found keeps its
/// edge, class body or no class body.
#[test]
fn an_anonymous_creation_on_a_class_still_reaches_its_constructor() {
    let scan = scan(&anonymous_class_tree());
    assert_eq!(
        scan.one("Base", RefKind::New),
        "RESOLVED com.acme#Base.<init>/0",
        "D-10 synthesises §8.8.9's implicit constructor, and it is the target",
    );
}

/// The three-level hierarchy the cross-file supertype cases are measured in.
///
/// One type per file on purpose: `extends` is the only fact a file states
/// about its own supertypes, so a hierarchy that fits in one compilation unit
/// is resolvable without a supertype phase at all and proves nothing about
/// one.
fn tower_tree() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "com/acme/Top.java",
            r#"package com.acme;
public class Top {
    public String top() { return "t"; }
}
"#,
        ),
        (
            "com/acme/Mid.java",
            r#"package com.acme;
public class Mid extends Top {
    public String mid() { return "m"; }
}
"#,
        ),
        (
            "com/acme/Low.java",
            r#"package com.acme;
public class Low extends Mid {
    public String low() { return "l"; }
}
"#,
        ),
    ]
}

/// H-01: a member declared two files above the receiver's type resolves.
///
/// The receiver is a *field* and not a parameter, here and in every H-01 case
/// below. A field is a node, so a reference through one stays in both terms of
/// the resolution rate; a parameter is not, and the uniform root-binding rule
/// on [`arthron::UnresolvedReason::LocalBinding`] answers it before the
/// supertype closure is ever consulted. Writing these on a parameter would
/// measure the policy instead of the closure.
#[test]
fn a_member_two_levels_up_a_cross_file_hierarchy_resolves() {
    let mut files = tower_tree();
    files.push((
        "com/acme/UseTower.java",
        r#"package com.acme;
public class UseTower {
    Low l;
    void go() {
        l.low();
        l.mid();
        l.top();
    }
}
"#,
    ));
    let scan = scan(&files);
    assert_eq!(
        scan.one("l.low", RefKind::Call),
        "RESOLVED com.acme#Low.low/0"
    );
    assert_eq!(
        scan.one("l.mid", RefKind::Call),
        "RESOLVED com.acme#Mid.mid/0"
    );
    assert_eq!(
        scan.one("l.top", RefKind::Call),
        "RESOLVED com.acme#Top.top/0"
    );
}

/// The closure adds targets; it never invents them. A name no type in the
/// hierarchy declares is still a miss, and still `UnindexedSupertype` —
/// `java.lang.Object` sits above every chain and is never indexed, so the
/// closure is short whatever this scan reads.
#[test]
fn a_member_no_type_in_the_hierarchy_declares_is_still_unindexed() {
    let mut files = tower_tree();
    files.push((
        "com/acme/UseAbsent.java",
        r#"package com.acme;
public class UseAbsent {
    Low l;
    void go() {
        l.absent();
    }
}
"#,
    ));
    let scan = scan(&files);
    assert_eq!(scan.one("l.absent", RefKind::Call), "UnindexedSupertype");
}

/// An interface's method, declared in a third file, reached through the class
/// that implements it — and a cycle in the hierarchy terminates.
#[test]
fn an_interface_method_resolves_and_a_cyclic_hierarchy_terminates() {
    let scan = scan(&[
        (
            "com/acme/Runner.java",
            r#"package com.acme;
public interface Runner {
    void run();
    default String describe() { return "runner"; }
}
"#,
        ),
        (
            "com/acme/Job.java",
            r#"package com.acme;
public class Job implements Runner {
    public void run() { }
}
"#,
        ),
        // Illegal Java, and the resolver still has to terminate on it: a
        // cycle in the store is a cycle whatever the compiler would say.
        (
            "com/acme/Loop.java",
            r#"package com.acme;
public class Loop extends Knot {
}
"#,
        ),
        (
            "com/acme/Knot.java",
            r#"package com.acme;
public class Knot extends Loop {
}
"#,
        ),
        (
            "com/acme/UseRunner.java",
            r#"package com.acme;
public class UseRunner {
    Job j;
    Loop k;
    void go() {
        j.describe();
        k.spin();
    }
}
"#,
        ),
    ]);
    assert_eq!(
        scan.one("j.describe", RefKind::Call),
        "RESOLVED com.acme#Runner.describe/0",
    );
    assert_eq!(scan.one("k.spin", RefKind::Call), "UnindexedSupertype");
}

/// §8.4.8.1: a superclass method beats a superinterface's default, and the
/// closure has to preserve that order across a file boundary.
///
/// The relation is stored in declaration order — §8.1.4 writes `extends`
/// before `implements` — and the walk is a stack, so getting this wrong is a
/// silent wrong edge to the interface rather than a lowered rate.
#[test]
fn a_superclass_method_beats_a_superinterface_default_across_files() {
    let scan = scan(&[
        (
            "com/acme/Chatty.java",
            r#"package com.acme;
public interface Chatty {
    default String speak() { return "iface"; }
}
"#,
        ),
        (
            "com/acme/BaseSpeak.java",
            r#"package com.acme;
public class BaseSpeak {
    public String speak() { return "class"; }
}
"#,
        ),
        (
            "com/acme/Both.java",
            r#"package com.acme;
public class Both extends BaseSpeak implements Chatty {
}
"#,
        ),
        (
            "com/acme/UseBoth.java",
            r#"package com.acme;
public class UseBoth {
    Both b;
    void go() {
        b.speak();
    }
}
"#,
        ),
    ]);
    assert_eq!(
        scan.one("b.speak", RefKind::Call),
        "RESOLVED com.acme#BaseSpeak.speak/0",
    );
}

/// An in-repository override of an `Object` member is an edge, not a link to
/// the JDK.
///
/// `is_object_member` answers for the member a search *failed* to find, and
/// before the closure that search stopped at the receiver's own type — so a
/// `toString` two files up was reported `External("jdk:java.lang")` and left
/// both terms of the rate. Now the search finds it, and the reference is
/// resolved: the movement is into the rate, never out of it.
#[test]
fn an_object_member_overridden_in_the_repository_resolves_rather_than_externalises() {
    let scan = scan(&[
        (
            "com/acme/Named.java",
            r#"package com.acme;
public class Named {
    public String toString() { return "named"; }
}
"#,
        ),
        (
            "com/acme/Sub.java",
            r#"package com.acme;
public class Sub extends Named {
}
"#,
        ),
        (
            "com/acme/UseNamed.java",
            r#"package com.acme;
public class UseNamed {
    Sub s;
    Object o;
    void go() {
        s.toString();
        o.hashCode();
    }
}
"#,
        ),
    ]);
    assert_eq!(
        scan.one("s.toString", RefKind::Call),
        "RESOLVED com.acme#Named.toString/0",
    );
    // The other direction is untouched: nothing in this repository declares
    // `hashCode` on `Object`, so it stays the JDK's.
    assert_eq!(
        scan.one("o.hashCode", RefKind::Call),
        "EXTERNAL jdk:java.lang",
    );
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

/// §8.4.8 and §9.4.1: a class inherits a default method from the *most
/// specific* superinterface that declares it. `Impl implements Alpha, Beta`
/// with `Beta extends Alpha` inherits `Beta.m`, whatever order the two are
/// written in — `javac` on this exact tree prints `beta`.
///
/// The closure walked the stored supertype list as a stack and returned the
/// first declaration it met, so declaration order decided the answer and the
/// two sides of a file boundary decided it differently. Both spellings are
/// asserted here, because an order-dependent walk passes one of them by luck.
#[test]
fn a_subinterfaces_default_beats_its_superinterfaces() {
    let tree = |implements: &str| {
        vec![
            (
                "com/acme/Alpha.java",
                r#"package com.acme;
public interface Alpha { default String m() { return "alpha"; } }
"#
                .to_string(),
            ),
            (
                "com/acme/Beta.java",
                r#"package com.acme;
public interface Beta extends Alpha { default String m() { return "beta"; } }
"#
                .to_string(),
            ),
            (
                "com/acme/Impl.java",
                format!("package com.acme;\npublic class Impl implements {implements} {{ }}\n"),
            ),
            (
                "com/acme/UseImpl.java",
                r#"package com.acme;
public class UseImpl {
    Impl impl;
    String go() { return impl.m(); }
}
"#
                .to_string(),
            ),
        ]
    };
    for implements in ["Alpha, Beta", "Beta, Alpha"] {
        let owned = tree(implements);
        let files: Vec<(&str, &str)> = owned.iter().map(|(p, s)| (*p, s.as_str())).collect();
        let scan = scan(&files);
        assert_eq!(
            scan.one("impl.m", RefKind::Call),
            "RESOLVED com.acme#Beta.m/0",
            "`implements {implements}` must still inherit the subinterface's default",
        );
    }
}

/// The same rule inside one file: the walk that reads `scope.supers` has to
/// agree with the one that reads the stored relation.
#[test]
fn a_subinterfaces_default_beats_its_superinterface_in_one_file() {
    let files = vec![
        (
            "com/acme/Alpha.java",
            r#"package com.acme;
public interface Alpha { default String m() { return "alpha"; } }
"#,
        ),
        (
            "com/acme/Beta.java",
            r#"package com.acme;
public interface Beta extends Alpha { default String m() { return "beta"; } }
"#,
        ),
        (
            "com/acme/Local.java",
            r#"package com.acme;
public class Local implements Alpha, Beta {
    String go() { return this.m(); }
}
"#,
        ),
    ];
    let scan = scan(&files);
    assert_eq!(
        scan.one("this.m", RefKind::Call),
        "RESOLVED com.acme#Beta.m/0",
    );
}

/// §8.2: a private member is not inherited, so a supertype's is not a
/// candidate below it. `javac` on this exact tree says `cannot find symbol`
/// for `hidden()` in `Sub` — which is the point: the closure used to answer
/// with `Base.hidden`, an edge into a body the subclass cannot name.
///
/// The tree does not compile, and that is the honest scope of the rule: in
/// source that does compile, no site names an inaccessible member, so this
/// changes no corpus number. It changes what arthron says about source that
/// is mid-edit, generated, or read without the module that completes it —
/// which is most of what a scanner meets.
#[test]
fn a_private_member_of_a_supertype_is_not_inherited() {
    let files = vec![
        (
            "com/acme/Hidden.java",
            r#"package com.acme;
public class Hidden {
    private String only() { return "hidden"; }
}
"#,
        ),
        (
            "com/acme/Below.java",
            r#"package com.acme;
public class Below extends Hidden {
    String go() { return this.only(); }
}
"#,
        ),
    ];
    let scan = scan(&files);
    assert_eq!(
        scan.one("this.only", RefKind::Call),
        "UnindexedSupertype",
        "a private supertype member is not a candidate, and the miss stays honest",
    );
}

/// The same member named on the type that declares it is an ordinary one.
/// The facet removes a member from *subtypes*, never from its own owner.
#[test]
fn a_private_member_still_resolves_on_the_type_that_declares_it() {
    let files = vec![(
        "com/acme/Owner.java",
        r#"package com.acme;
public class Owner {
    private String only() { return "owner"; }
    String go() { return this.only(); }
}
"#,
    )];
    let scan = scan(&files);
    assert_eq!(
        scan.one("this.only", RefKind::Call),
        "RESOLVED com.acme#Owner.only/0",
    );
}
