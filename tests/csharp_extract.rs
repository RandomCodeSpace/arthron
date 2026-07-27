//! C# extraction, one fixture per construct.
//!
//! What these assert is the **tier-2 contract**: definitions, structure and
//! imports, and nothing else. A C# extractor that emitted a call site or a
//! type use would put references into a denominator no tier-2 resolver links,
//! which is tier-1 coverage claimed without tier-1 work — so the reference
//! kinds this file allows are checked as an invariant rather than as a detail
//! of one fixture.

use arthron::model::{DeclSpace, DefKind, Definition, Params, RefKind, Reference};
use arthron::track_csharp::extract::{ImportForm, extract};

/// `(kind, owner joined by `/`, name)` for every definition, in source order.
fn defs(source: &str) -> Vec<(DefKind, String, String)> {
    extract("src/F.cs", source)
        .defs
        .iter()
        .map(|d: &Definition| (d.kind, d.owner.join("/"), d.name.clone()))
        .collect()
}

/// `(kind, space, raw target, segments joined by `.`)` for every reference.
fn refs(source: &str) -> Vec<(RefKind, DeclSpace, String, String)> {
    extract("src/F.cs", source)
        .refs
        .iter()
        .map(|r: &Reference| {
            (
                r.kind,
                r.space,
                r.raw_target.clone(),
                r.target.segments.join("."),
            )
        })
        .collect()
}

/// The import clauses a file states, as `(form, global)`.
fn forms(source: &str) -> Vec<(ImportForm, bool)> {
    extract("src/F.cs", source)
        .header
        .imports
        .iter()
        .map(|i| (i.form.clone(), i.global))
        .collect()
}

fn segs(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn a_file_scoped_namespace_owns_everything_after_it() {
    let facts = extract("src/Log.cs", "namespace Serilog.Core;\n\nclass Logger {}\n");
    assert_eq!(facts.header.namespaces, ["Serilog.Core"]);
    assert_eq!(
        defs("namespace Serilog.Core;\n\nclass Logger {}\n"),
        [
            // The namespace as written, then the one it implies. `namespace
            // A.B;` declares `A` too — C# spells nested namespaces this way
            // — so `using Serilog;` names something this file created.
            (DefKind::Module, String::new(), "Serilog.Core".to_string()),
            (DefKind::Module, String::new(), "Serilog".to_string()),
            (
                DefKind::Type,
                "Serilog.Core".to_string(),
                "Logger".to_string()
            ),
        ],
    );
}

#[test]
fn a_braced_namespace_owns_only_its_own_block() {
    // Guard.cs's shape: a braced namespace, then a type outside it, which
    // lands in the global namespace whatever the block above said.
    let out = defs(concat!(
        "namespace JetBrains.Annotations\n{\n    sealed class NoEnumerationAttribute {}\n}\n",
        "\nstatic class Guard {}\n",
    ));
    assert_eq!(
        out,
        [
            (
                DefKind::Module,
                String::new(),
                "JetBrains.Annotations".to_string()
            ),
            (DefKind::Module, String::new(), "JetBrains".to_string()),
            (
                DefKind::Type,
                "JetBrains.Annotations".to_string(),
                "NoEnumerationAttribute".to_string()
            ),
            (DefKind::Type, String::new(), "Guard".to_string()),
        ],
    );
}

#[test]
fn a_file_that_declares_no_namespace_lands_in_the_global_one() {
    let facts = extract("src/GlobalUsings.cs", "global using System.Text;\n");
    // One module, and its name is the empty string: the global namespace is
    // a container with no name, not the absence of one.
    assert_eq!(facts.header.namespaces, [""]);
    assert_eq!(
        facts.defs.iter().map(|d| d.kind).collect::<Vec<_>>(),
        [DefKind::Module],
    );
}

#[test]
fn every_type_declaration_form_is_a_type() {
    let out = defs(concat!(
        "namespace N;\n",
        "class C {}\nstruct S {}\ninterface I {}\nenum E {}\n",
        "record R(int A);\ndelegate void D(int x);\n",
    ));
    let types: Vec<(DefKind, String, String)> = out
        .into_iter()
        .filter(|(k, _, _)| *k == DefKind::Type)
        .collect();
    assert_eq!(
        types,
        [
            (DefKind::Type, "N".to_string(), "C".to_string()),
            (DefKind::Type, "N".to_string(), "S".to_string()),
            (DefKind::Type, "N".to_string(), "I".to_string()),
            (DefKind::Type, "N".to_string(), "E".to_string()),
            (DefKind::Type, "N".to_string(), "R".to_string()),
            (DefKind::Type, "N".to_string(), "D".to_string()),
        ],
    );
}

#[test]
fn a_nested_type_carries_its_outer_types_in_its_owner() {
    assert!(
        defs("namespace N;\nclass Outer { struct Inner {} }\n").contains(&(
            DefKind::Type,
            "N/Outer".to_string(),
            "Inner".to_string()
        ))
    );
}

#[test]
fn members_are_kinded_by_what_a_reference_can_do_with_them() {
    let out = defs(concat!(
        "namespace N;\nclass C\n{\n",
        "    const int Max = 3;\n",
        "    static readonly int Field2 = 1;\n",
        "    int _a, _b;\n",
        "    public event System.EventHandler? Changed;\n",
        "    public string Name { get; set; }\n",
        "    public int this[int i] => i;\n",
        "    public C(int x) {}\n",
        "    public void Write(string s) {}\n",
        "}\n",
    ));
    let members: Vec<(DefKind, String)> = out
        .into_iter()
        .filter(|(k, _, _)| *k != DefKind::Module && *k != DefKind::Type)
        .map(|(k, _, n)| (k, n))
        .collect();
    assert_eq!(
        members,
        [
            // A `const` field is a constant; a `static readonly` one is not.
            (DefKind::Const, "Max".to_string()),
            (DefKind::Field, "Field2".to_string()),
            // One declaration, two declarators, two fields.
            (DefKind::Field, "_a".to_string()),
            (DefKind::Field, "_b".to_string()),
            // An event is the add/remove pair a `+=` names, never its
            // backing field, so it is filed the way a property is.
            (DefKind::Property, "Changed".to_string()),
            (DefKind::Property, "Name".to_string()),
            (DefKind::Property, "this[]".to_string()),
            (DefKind::Constructor, "C".to_string()),
            (DefKind::Method, "Write".to_string()),
        ],
    );
}

#[test]
fn an_enum_member_is_a_constant_of_its_enum() {
    let out = defs("namespace N;\nenum Level { Info, Warn = 2 }\n");
    assert!(out.contains(&(DefKind::Const, "N/Level".to_string(), "Info".to_string())));
    assert!(out.contains(&(DefKind::Const, "N/Level".to_string(), "Warn".to_string())));
}

#[test]
fn a_positional_record_declares_a_property_per_parameter() {
    let out =
        defs("namespace N;\npublic readonly record struct F(object Sender, string Message);\n");
    assert!(out.contains(&(DefKind::Property, "N/F".to_string(), "Sender".to_string())));
    assert!(out.contains(&(DefKind::Property, "N/F".to_string(), "Message".to_string())));
    // A class primary constructor is *not* a property declaration — C# makes
    // that promotion for records only — so nothing is minted for one.
    let plain = defs("namespace N;\npublic class P(int x) { }\n");
    assert!(
        !plain
            .iter()
            .any(|(k, _, n)| *k == DefKind::Property && n == "x"),
        "{plain:?}"
    );
}

#[test]
fn a_local_function_is_not_a_member() {
    let out = defs("namespace N;\nclass C { void M() { void Local() {} } }\n");
    assert!(!out.iter().any(|(_, _, n)| n == "Local"), "{out:?}");
}

#[test]
fn a_member_whose_enclosing_type_the_parser_lost_is_not_invented() {
    // The corpus's own shape, measured: a `#if` that splits a method's
    // *signature* from its body is more than tree-sitter-c-sharp's error
    // recovery can carry. One or two of them in a type recover cleanly;
    // the third collapses the enclosing type declaration into an `ERROR`
    // node. The methods survive the recovery, their owner does not, and no
    // owner is guessed for them — C# has no member outside a type, so an
    // empty owner chain means the tree is an artefact.
    let mut source = String::from("namespace N;\nclass C\n{\n");
    for i in 0..3 {
        source.push_str(&format!(
            "#if FEATURE_SPAN\n    void M{i}(int a)\n#else\n    void M{i}(string a)\n#endif\n    {{\n    }}\n",
        ));
    }
    source.push_str("}\n");
    let out = defs(&source);
    // The namespace still parses, and nothing below it does.
    assert_eq!(
        out,
        [(DefKind::Module, String::new(), "N".to_string())],
        "a lost owner was invented",
    );
    // Two of them is inside the budget, and then both arms declare.
    let ok = defs(concat!(
        "namespace N;\nclass C\n{\n",
        "#if FEATURE_SPAN\n    void M(int a)\n#else\n    void M(string a)\n#endif\n    { }\n}\n",
    ));
    assert_eq!(
        ok.iter().filter(|(k, _, _)| *k == DefKind::Method).count(),
        2,
        "{ok:?}",
    );
}

#[test]
fn both_arms_of_a_conditional_declare() {
    // No preprocessing: the extractor reads the file as written, so a member
    // declared under `#if` and another under `#else` are both declarations.
    // Choosing an arm would mean choosing a target framework, and this
    // repository's `.csproj` names seven.
    let out = defs(concat!(
        "namespace N;\nclass C\n{\n",
        "#if FEATURE_SPAN\n    public void A() {}\n#else\n    public void B() {}\n#endif\n}\n",
    ));
    assert!(out.contains(&(DefKind::Method, "N/C".to_string(), "A".to_string())));
    assert!(out.contains(&(DefKind::Method, "N/C".to_string(), "B".to_string())));
}

#[test]
fn a_plain_using_names_a_namespace() {
    assert_eq!(
        refs("using System.Diagnostics;\nnamespace N;\n"),
        [(
            RefKind::Import,
            DeclSpace::Namespace,
            "using System.Diagnostics".to_string(),
            "System.Diagnostics".to_string(),
        )],
    );
    assert_eq!(
        forms("using System.Diagnostics;\n"),
        [(
            ImportForm::Namespace(segs(&["System", "Diagnostics"])),
            false
        )],
    );
}

#[test]
fn a_global_using_is_the_same_import_with_a_wider_scope() {
    assert_eq!(
        refs("global using Serilog.Events;\n"),
        [(
            RefKind::Import,
            DeclSpace::Namespace,
            "global using Serilog.Events".to_string(),
            "Serilog.Events".to_string(),
        )],
    );
    assert_eq!(
        forms("global using Serilog.Events;\n"),
        [(ImportForm::Namespace(segs(&["Serilog", "Events"])), true)],
    );
}

#[test]
fn a_static_using_names_a_type() {
    assert_eq!(
        refs("global using static Serilog.Events.LogEventLevel;\n"),
        [(
            RefKind::Import,
            DeclSpace::Type,
            "global using static Serilog.Events.LogEventLevel".to_string(),
            "Serilog.Events.LogEventLevel".to_string(),
        )],
    );
    assert_eq!(
        forms("global using static Serilog.Events.LogEventLevel;\n"),
        [(
            ImportForm::Static(segs(&["Serilog", "Events", "LogEventLevel"])),
            true
        )],
    );
}

#[test]
fn an_alias_using_names_either_a_type_or_a_namespace() {
    assert_eq!(
        refs("using File = System.IO.File;\n"),
        [(
            RefKind::Import,
            // A name the site does not say which table to read in: C# lets
            // `using X = A.B;` alias a type *or* a namespace, so the space is
            // the one the resolver has to try both of.
            DeclSpace::Type,
            "using File = System.IO.File".to_string(),
            "System.IO.File".to_string(),
        )],
    );
    assert_eq!(
        forms("using File = System.IO.File;\n"),
        [(
            ImportForm::Alias {
                alias: "File".to_string(),
                target: segs(&["System", "IO", "File"]),
            },
            false
        )],
    );
}

#[test]
fn the_second_meaning_of_using_is_not_an_import() {
    // `using (…)` and `using var …` dispose a resource. They name a local,
    // not a namespace, and tier 2 emits no expression-level reference at all.
    let out = refs(concat!(
        "namespace N;\nclass C { void M() {\n",
        "  using var w = Get();\n  using (var x = Get()) { }\n} }\n",
    ));
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn an_extern_alias_names_an_assembly_and_so_names_no_node() {
    // `extern alias Foo;` names an assembly alias the build system declares
    // in the `.csproj`, not a namespace or a type any source file declares.
    // There is no node for it to name, so no reference is emitted rather
    // than one that could only ever miss.
    assert!(refs("extern alias Foo;\nnamespace N;\n").is_empty());
}

#[test]
fn the_only_reference_kind_is_an_import() {
    // The invariant, on a file that exercises every shape at once: a call, a
    // `new`, a base list and a type annotation are all in this source and
    // none of them is a reference.
    let facts = extract(
        "src/F.cs",
        concat!(
            "using System.Text;\nnamespace N;\n",
            "class C : System.IDisposable\n{\n",
            "    StringBuilder _b = new StringBuilder();\n",
            "    public void Dispose() { _b.Clear(); Other.Go(); }\n}\n",
        ),
    );
    assert_eq!(facts.refs.len(), 1, "{:?}", facts.refs);
    for r in &facts.refs {
        assert_eq!(r.kind, RefKind::Import);
        // Tier 2 emits nothing a block could bind, so the bucket that sits
        // outside both terms of the rate stays structurally empty.
        assert!(!r.locally_bound);
    }
}

#[test]
fn records_come_out_in_source_order() {
    let facts = extract(
        "src/F.cs",
        "using A.B;\nnamespace N;\nclass C { void M() {} }\nclass D {}\n",
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
    );
}

#[test]
fn an_import_clause_and_its_reference_are_paired_by_span() {
    let facts = extract(
        "src/F.cs",
        concat!(
            "using System.Text;\nusing static System.Math;\n",
            "using F = System.IO.File;\nglobal using Serilog;\n",
        ),
    );
    assert_eq!(facts.refs.len(), 4);
    assert_eq!(facts.header.imports.len(), 4);
    for (r, i) in facts.refs.iter().zip(&facts.header.imports) {
        assert_eq!(
            (r.span.byte_start, r.span.byte_end),
            (i.span.byte_start, i.span.byte_end)
        );
    }
}

#[test]
fn a_broken_file_still_yields_its_namespace_and_its_imports() {
    // tree-sitter is error-tolerant, and a file that does not parse is still
    // a file whose `using` directives name real namespaces.
    let facts = extract(
        "src/Broken.cs",
        "using System.Text;\nnamespace N;\nclass ((( \n",
    );
    assert_eq!(facts.header.namespaces, ["N"]);
    assert_eq!(facts.refs.len(), 1);
}

#[test]
fn a_params_parameter_is_still_a_parameter() {
    // tree-sitter-c-sharp does not wrap a `params` parameter in a `parameter`
    // node the way it wraps every other one: it flattens the keyword, the
    // type and the name straight into the list. Reading only `parameter`
    // children therefore loses it — and `Push(params ReadOnlySpan<T>)`,
    // `Push(params T[])` and `Push(params IEnumerable<T>)`, all three of
    // which the corpus declares side by side, would hash to one node.
    let shapes: Vec<Option<Params>> = extract(
        "src/F.cs",
        concat!(
            "namespace N;\nclass C\n{\n",
            "    void Push(params ReadOnlySpan<int> e) {}\n",
            "    void Push(params int[] e) {}\n",
            "    void Debug(string t, params object?[]? values) {}\n",
            "    void Plain(string t) {}\n",
            "}\n",
        ),
    )
    .defs
    .into_iter()
    .filter(|d| d.kind == DefKind::Method)
    .map(|d| d.params)
    .collect();
    assert_eq!(
        shapes,
        [
            Some(Params {
                count: 1,
                varargs: true,
                types: vec!["ReadOnlySpan<int>".to_string()]
            }),
            Some(Params {
                count: 1,
                varargs: true,
                types: vec!["int[]".to_string()]
            }),
            Some(Params {
                count: 2,
                varargs: true,
                types: vec!["string".to_string(), "object?[]?".to_string()],
            }),
            Some(Params {
                count: 1,
                varargs: false,
                types: vec!["string".to_string()]
            }),
        ],
    );
}

#[test]
fn a_parameters_modifiers_and_attributes_are_not_part_of_its_type() {
    let shapes: Vec<Option<Params>> = extract(
        "src/F.cs",
        concat!(
            "namespace N;\nclass C\n{\n",
            "    void M(ref readonly int x, [Attr] string? y = null, out long z) {}\n",
            "}\n",
        ),
    )
    .defs
    .into_iter()
    .filter(|d| d.kind == DefKind::Method)
    .map(|d| d.params)
    .collect();
    assert_eq!(
        shapes,
        [Some(Params {
            count: 3,
            varargs: false,
            types: vec!["int".to_string(), "string?".to_string(), "long".to_string()],
        })],
    );
}

#[test]
fn an_explicit_interface_implementation_is_its_own_member() {
    // `IEnumerable<T>.GetEnumerator()` is a different member from the public
    // `GetEnumerator()` beside it — a type may declare both, and the corpus
    // declares three of them in one struct. The name carries the interface
    // the way .NET metadata does, because it is the only thing that tells
    // them apart.
    let out = defs(concat!(
        "namespace N;\nstruct S\n{\n",
        "    public Enumerator GetEnumerator() => new(this);\n",
        "    IEnumerator<T> IEnumerable<T>.GetEnumerator() => new Enumerator(this);\n",
        "    IEnumerator IEnumerable.GetEnumerator() => new Enumerator(this);\n",
        "    public T Current => _c;\n",
        "    object IEnumerator.Current => _c;\n",
        "}\n",
    ));
    let members: Vec<(DefKind, String)> = out
        .into_iter()
        .filter(|(k, _, _)| *k != DefKind::Module && *k != DefKind::Type)
        .map(|(k, _, n)| (k, n))
        .collect();
    assert_eq!(
        members,
        [
            (DefKind::Method, "GetEnumerator".to_string()),
            (DefKind::Method, "IEnumerable<T>.GetEnumerator".to_string()),
            (DefKind::Method, "IEnumerable.GetEnumerator".to_string()),
            (DefKind::Property, "Current".to_string()),
            (DefKind::Property, "IEnumerator.Current".to_string()),
        ],
    );
}

#[test]
fn an_alias_whose_bound_name_repeats_its_target_still_names_the_target() {
    // `using File = File;` writes the bound name and the target as two
    // identical name-shaped children, and only their position tells them
    // apart. Reading the last one that is not spelled like the alias would
    // find nothing here and emit no reference at all.
    assert_eq!(
        forms("using File = File;\n"),
        [(
            ImportForm::Alias {
                alias: "File".to_string(),
                target: segs(&["File"]),
            },
            false
        )],
    );
}

#[test]
fn an_operator_declaration_is_named_the_way_the_source_names_it() {
    // The metadata names are `op_Addition` and `op_Implicit`; translating to
    // them is a table, and nothing in the corpus declares an operator, so no
    // table is written here. The source spelling is the fact the file states.
    let out = defs(concat!(
        "namespace N;\nclass C\n{\n",
        "    public static C operator +(C a, C b) => a;\n",
        "    public static implicit operator string(C a) => \"\";\n",
        "    ~C() {}\n",
        "}\n",
    ));
    let names: Vec<String> = out
        .into_iter()
        .filter(|(k, _, _)| *k == DefKind::Method)
        .map(|(_, _, n)| n)
        .collect();
    assert_eq!(names, ["operator +", "implicit operator string", "~C"],);
}
