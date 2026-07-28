//! C# resolution, one fixture per rule.
//!
//! Every `using` ends `Resolved`, `External`, or `Unresolved` with a reason
//! from the ratified taxonomy; there is no way to express "dropped", and no
//! reason was added for C#.

use std::collections::HashSet;

use arthron::lang::Resolver;
use arthron::model::{
    DeclSpace, DefFacets, DefKind, Definition, Domain, Fqn, NodeId, Params, Span, node_id,
};
use arthron::track_csharp::extract::extract;
use arthron::track_csharp::resolve::{CsProject, CsResolver};
use arthron::{Outcome, UnresolvedReason};

/// A symbol table holding exactly the FQNs named.
fn table(fqns: &[&str]) -> HashSet<NodeId> {
    fqns.iter().map(|f| node_id(Domain::CSharp, f)).collect()
}

/// Resolve every reference in one file against a table, as `(raw target,
/// outcome)`.
fn outcomes(source: &str, known: &[&str]) -> Vec<(String, Outcome<NodeId, String>)> {
    let table = table(known);
    let facts = extract("src/F.cs", source);
    let scope = CsResolver.scope(&CsProject, &facts, &table);
    facts
        .refs
        .iter()
        .map(|r| {
            (
                r.raw_target.clone(),
                CsResolver.resolve(&CsProject, &scope, r, &table).outcome,
            )
        })
        .collect()
}

fn id(fqn: &str) -> NodeId {
    node_id(Domain::CSharp, fqn)
}

fn def(kind: DefKind, name: &str, owner: &[&str], params: Option<Params>) -> Definition {
    Definition {
        kind,
        name: name.to_string(),
        owner: owner.iter().map(|s| (*s).to_string()).collect(),
        space: DeclSpace::Value,
        facets: DefFacets::default(),
        params,
        span: Span {
            byte_start: 0,
            byte_end: 0,
            line: 1,
        },
    }
}

fn fqn_of(def: &Definition) -> Option<String> {
    let table: HashSet<NodeId> = HashSet::new();
    CsResolver
        .def_fqn(
            &CsProject,
            &extract("src/F.cs", "").header,
            &def.owner,
            def,
            &table,
        )
        .map(Fqn::into_string)
}

#[test]
fn a_plain_using_resolves_to_a_namespace_this_repository_declares() {
    assert_eq!(
        outcomes("using Serilog.Events;\n", &["Serilog.Events"]),
        [(
            "using Serilog.Events".to_string(),
            Outcome::Resolved(id("Serilog.Events")),
        )],
    );
}

#[test]
fn a_namespace_this_repository_does_not_declare_is_another_assemblys() {
    // Rule 2. C# namespaces are open and no repository owns a prefix, so the
    // only question a plain `using` asks is whether *this* repository
    // declares the name — and the answer here is a probe, never a claim on a
    // subtree.
    assert_eq!(
        outcomes("using System.Diagnostics;\n", &["Serilog.Events"]),
        [(
            "using System.Diagnostics".to_string(),
            Outcome::External("System".to_string()),
        )],
    );
}

#[test]
fn a_repository_that_declares_a_polyfill_namespace_does_not_claim_the_rest_of_it() {
    // The corpus's own shape: `src/Serilog/Util/TimeProvider.cs` declares
    // `namespace System;` under `#if !NET8_0_OR_GREATER`. A prefix-claim rule
    // would call every one of the corpus's 33 `System.*` imports an
    // in-repository miss. This is the test that says it does not.
    assert_eq!(
        outcomes("using System.Diagnostics;\n", &["System"]),
        [(
            "using System.Diagnostics".to_string(),
            Outcome::External("System".to_string()),
        )],
    );
}

#[test]
fn a_using_of_a_namespace_only_implied_by_a_declaration_resolves() {
    // `namespace Serilog.Settings.KeyValuePairs;` declares `Serilog.Settings`
    // too, and a file may name it without any file spelling it alone.
    let facts = extract("src/S.cs", "namespace Serilog.Settings.KeyValuePairs;\n");
    let names: Vec<&str> = facts.defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        [
            // The global namespace every file begins in, then the declaration
            // and the two it implies.
            "",
            "Serilog.Settings.KeyValuePairs",
            "Serilog.Settings",
            "Serilog"
        ],
    );
    assert_eq!(
        outcomes("using Serilog.Settings;\n", &["Serilog.Settings"]),
        [(
            "using Serilog.Settings".to_string(),
            Outcome::Resolved(id("Serilog.Settings")),
        )],
    );
}

#[test]
fn a_using_of_a_nested_namespace_block_is_ours_and_not_another_assemblys() {
    // The classification that hurts most when the extractor gets a namespace
    // name wrong: `External` sits outside both terms of the rate, so a name
    // this repository declares and the resolver could not find would raise no
    // miss and cost no rate. `namespace Alpha { namespace Beta { … } }`
    // declares `Alpha.Beta`, and this is what says so end to end.
    assert_eq!(
        outcomes("using Alpha.Beta;\n", &["Alpha.Beta"]),
        [(
            "using Alpha.Beta".to_string(),
            Outcome::Resolved(id("Alpha.Beta")),
        )],
    );
    let facts = extract(
        "src/Outer.cs",
        "namespace Alpha\n{\n    namespace Beta\n    {\n        class Widget {}\n    }\n}\n",
    );
    let widget = facts
        .defs
        .iter()
        .find(|d| d.name == "Widget")
        .expect("the nested type is extracted");
    assert_eq!(fqn_of(widget), Some("Alpha.Beta#Widget".to_string()));
}

#[test]
fn an_alias_to_a_generic_type_reaches_the_declaration_in_this_repository() {
    // `class Box<T>` is stored as ``Ns#Box`1``. An import segment that
    // dropped the arity would probe `Ns#Box` and miss a type named as plainly
    // as C# can name one.
    assert_eq!(
        outcomes("using B = Ns.Box<int>;\n", &["Ns#Box`1"]),
        [(
            "using B = Ns.Box`1".to_string(),
            Outcome::Resolved(id("Ns#Box`1")),
        )],
    );
    assert_eq!(
        fqn_of(&def(
            DefKind::Type,
            "Box",
            &["Ns"],
            Some(Params {
                count: 1,
                varargs: false,
                types: Vec::new(),
            }),
        )),
        Some("Ns#Box`1".to_string()),
        "the key the alias probes is the key the declaration is filed under",
    );
}

#[test]
fn a_one_segment_static_using_is_our_own_miss_whatever_else_the_repository_holds() {
    // Rule 5 reads `using static Math;` as a type in the global namespace,
    // which every C# repository has — so the miss is `NoMatchingDefinition`,
    // the answer that counts *against* the rate, and never `External`, which
    // would launder it out of both terms.
    //
    // The reason this is a fixture and not a remark: the global namespace
    // used to be minted only by a file that declared no namespace of its own,
    // so this very line was `External` in a repository where every file
    // declared one and a miss in a repository with a single `GlobalUsings.cs`
    // beside it. A table holding the global namespace is now what every C#
    // scan produces, so only one of the two answers is reachable.
    assert_eq!(
        outcomes("using static Math;\n", &[""]),
        [(
            "using static Math".to_string(),
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
        )],
    );
    // And the extractor puts `""` in the table for a file that declares a
    // namespace, which is the case that used to answer `External`.
    let facts = extract(
        "src/A.cs",
        "using static Math;\nnamespace Ns;\nclass Inner {}\n",
    );
    assert!(
        facts
            .defs
            .iter()
            .any(|d| d.kind == DefKind::Module && d.name.is_empty()),
        "the global namespace is not declared by a file that declares another",
    );
}

#[test]
fn a_static_using_resolves_to_a_type() {
    assert_eq!(
        outcomes(
            "global using static Serilog.Events.LogEventLevel;\n",
            &["Serilog.Events#LogEventLevel"],
        ),
        [(
            "global using static Serilog.Events.LogEventLevel".to_string(),
            Outcome::Resolved(id("Serilog.Events#LogEventLevel")),
        )],
    );
}

#[test]
fn a_static_using_reaches_a_nested_type() {
    // Rule 3 probes every namespace/type split, longest namespace first, so
    // `A.B.C.D` finds `A.B#C+D` when `A.B.C` is a type rather than a
    // namespace.
    assert_eq!(
        outcomes("using static A.B.C.D;\n", &["A.B#C+D"]),
        [(
            "using static A.B.C.D".to_string(),
            Outcome::Resolved(id("A.B#C+D")),
        )],
    );
}

#[test]
fn an_alias_resolves_to_a_type_or_to_a_namespace() {
    assert_eq!(
        outcomes(
            "using P = Serilog.Parsing.PropertyToken;\n",
            &["Serilog.Parsing#PropertyToken"],
        ),
        [(
            "using P = Serilog.Parsing.PropertyToken".to_string(),
            Outcome::Resolved(id("Serilog.Parsing#PropertyToken")),
        )],
    );
    // C# lets an alias bind a namespace just as readily, and nothing at the
    // site says which, so both tables are tried.
    assert_eq!(
        outcomes("using P = Serilog.Parsing;\n", &["Serilog.Parsing"]),
        [(
            "using P = Serilog.Parsing".to_string(),
            Outcome::Resolved(id("Serilog.Parsing")),
        )],
    );
}

#[test]
fn a_type_missing_from_a_namespace_this_repository_declares_is_our_own_miss() {
    // Rule 5. The container is ours and the name is not in it, which is the
    // one case the reason reserved for meaning *our* bug describes.
    assert_eq!(
        outcomes(
            "using static Serilog.Events.Absent;\n",
            &["Serilog.Events", "Serilog"],
        ),
        [(
            "using static Serilog.Events.Absent".to_string(),
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
        )],
    );
    // And a type whose container is nobody's here is outside the repository.
    assert_eq!(
        outcomes("using F = System.IO.File;\n", &["System"]),
        [(
            "using F = System.IO.File".to_string(),
            Outcome::External("System".to_string()),
        )],
    );
}

#[test]
fn every_probe_is_recorded_hit_or_miss() {
    // The candidate list feeds the invalidation index, so it must list
    // exactly what was read and nothing else.
    let known = table(&["A.B#C+D"]);
    let facts = extract("src/F.cs", "using static A.B.C.D;\n");
    let scope = CsResolver.scope(&CsProject, &facts, &known);
    let resolution = CsResolver.resolve(&CsProject, &scope, &facts.refs[0], &known);
    assert_eq!(
        resolution.candidates,
        [id("A.B.C#D"), id("A.B#C+D")],
        "the probes read, in read order",
    );
}

#[test]
fn a_namespace_and_a_type_of_one_spelling_are_two_identities() {
    assert_eq!(
        fqn_of(&def(DefKind::Module, "Serilog.Core", &[], None)),
        Some("Serilog.Core".to_string()),
    );
    assert_eq!(
        fqn_of(&def(DefKind::Type, "Core", &["Serilog"], None)),
        Some("Serilog#Core".to_string()),
    );
}

#[test]
fn a_generic_types_arity_is_part_of_its_identity() {
    let one = def(
        DefKind::Type,
        "Visitor",
        &["Serilog.Data"],
        Some(Params {
            count: 1,
            varargs: false,
            types: vec!["TState".to_string()],
        }),
    );
    let two = def(
        DefKind::Type,
        "Visitor",
        &["Serilog.Data"],
        Some(Params {
            count: 2,
            varargs: false,
            types: vec!["TState".to_string(), "TResult".to_string()],
        }),
    );
    assert_eq!(fqn_of(&one), Some("Serilog.Data#Visitor`1".to_string()));
    assert_eq!(fqn_of(&two), Some("Serilog.Data#Visitor`2".to_string()));
    assert_ne!(fqn_of(&one), fqn_of(&two));
}

#[test]
fn two_overloads_are_two_nodes() {
    let a = def(
        DefKind::Method,
        "Write",
        &["Serilog", "ILogger"],
        Some(Params {
            count: 1,
            varargs: false,
            types: vec!["string".to_string()],
        }),
    );
    let b = def(
        DefKind::Method,
        "Write",
        &["Serilog", "ILogger"],
        Some(Params {
            count: 1,
            varargs: false,
            types: vec!["LogEvent".to_string()],
        }),
    );
    assert_eq!(
        fqn_of(&a),
        Some("Serilog#ILogger::Write(string)".to_string())
    );
    assert_eq!(
        fqn_of(&b),
        Some("Serilog#ILogger::Write(LogEvent)".to_string()),
    );
    assert!(!CsResolver.mergeable(&a, &b));
}

#[test]
fn a_partial_type_written_twice_is_one_entity() {
    let a = def(DefKind::Type, "Logger", &["Serilog.Core"], None);
    let b = def(DefKind::Type, "Logger", &["Serilog.Core"], None);
    assert!(CsResolver.mergeable(&a, &b));
    // A class and an interface of one name in one namespace never
    // co-compile, and merging them would let one's sites stand in for the
    // other's — but they differ in kind, not in name, so nothing merges here
    // that should not.
    let c = def(DefKind::Method, "Logger", &["Serilog.Core", "T"], None);
    assert!(!CsResolver.mergeable(&a, &c));
}

#[test]
fn a_member_with_no_enclosing_type_is_not_nameable() {
    // C# has no member outside a type, so this shape is a recovery artefact
    // and gets no node rather than an invented owner.
    assert_eq!(fqn_of(&def(DefKind::Method, "M", &["N"], None)), None);
}

#[test]
fn a_nested_type_and_its_members_spell_the_metadata_path() {
    assert_eq!(
        fqn_of(&def(
            DefKind::Type,
            "Enumerator",
            &["Serilog.Context", "EnricherStack"],
            None,
        )),
        Some("Serilog.Context#EnricherStack+Enumerator".to_string()),
    );
    assert_eq!(
        fqn_of(&def(
            DefKind::Property,
            "Current",
            &["Serilog.Context", "EnricherStack", "Enumerator"],
            None,
        )),
        Some("Serilog.Context#EnricherStack+Enumerator::Current".to_string()),
    );
}

#[test]
fn the_global_namespace_is_a_container_with_no_name() {
    assert_eq!(
        fqn_of(&def(DefKind::Module, "", &[], None)),
        Some(String::new())
    );
    assert_eq!(
        fqn_of(&def(DefKind::Type, "Guard", &[""], None)),
        Some("#Guard".to_string()),
    );
}

#[test]
fn no_manifest_means_no_fingerprint_and_no_invalidation() {
    assert!(CsResolver.config_digest(&CsProject).is_empty());
    // And no supertype phase: tier 2 emits no inheritance reference.
    assert!(CsResolver.link_kinds().is_empty());
}
