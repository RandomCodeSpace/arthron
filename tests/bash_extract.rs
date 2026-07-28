//! Extractor fixtures for the Bash track: what one file yields, and what it
//! must never yield.
//!
//! The tier-2 contract is the first thing asserted here and the last thing
//! that may quietly change: **definitions, structure, and import-like
//! references only**. A `RefKind::Call` emitted by this extractor would put
//! references into a denominator nothing in this track resolves, and report
//! tier-1 coverage nobody measured.

use arthron::model::{DeclSpace, DefFacets, DefKind, RefKind, TargetRoot};
use arthron::track_bash::extract::{SourceForm, extract};

/// Every source form the fixtures below spell, in source order.
fn forms(source: &str) -> Vec<SourceForm> {
    extract("x.bash", source)
        .header
        .sources
        .into_iter()
        .map(|s| s.form)
        .collect()
}

/// The literal a single-clause fixture resolves to, or `None` when the
/// specifier is not one.
fn spec(source: &str) -> Option<String> {
    match forms(source).into_iter().next().expect("one clause") {
        SourceForm::Literal(path) => Some(path),
        SourceForm::Dynamic => None,
    }
}

#[test]
fn every_file_declares_the_script_a_source_can_name() {
    let facts = extract("lib/util.bash", "echo hi\n");
    let script = facts.defs.first().expect("a script node");
    assert_eq!(script.kind, DefKind::Module);
    assert_eq!(script.name, "util.bash");
    assert!(script.owner.is_empty());
    assert_eq!(script.space, DeclSpace::Namespace);
    assert!(script.facets.contains(DefFacets::SYNTHETIC));
    assert_eq!(script.span.line, 1);
}

#[test]
fn a_broken_file_still_yields_its_script_node() {
    // tree-sitter is error-tolerant, and a file that does not parse is still
    // a file a `source` can name.
    let facts = extract("lib/broken.sh", "if [ ; then\n");
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].name, "broken.sh");
}

#[test]
fn both_function_spellings_declare_one_function() {
    for source in [
        "hi() {\n  echo hi\n}\n",
        "function hi {\n  echo hi\n}\n",
        "function hi() {\n  echo hi\n}\n",
        "hi() ( echo hi )\n",
    ] {
        let facts = extract("lib/util.bash", source);
        let fns: Vec<_> = facts
            .defs
            .iter()
            .filter(|d| d.kind == DefKind::Function)
            .collect();
        assert_eq!(fns.len(), 1, "{source:?} -> {:?}", facts.defs);
        assert_eq!(fns[0].name, "hi");
        assert!(fns[0].owner.is_empty(), "{:?}", fns[0].owner);
        assert_eq!(fns[0].space, DeclSpace::Value);
    }
}

#[test]
fn a_nested_function_carries_the_function_that_writes_it() {
    let facts = extract("lib/util.bash", "outer() {\n  inner() {\n    :\n  }\n}\n");
    let names: Vec<(&str, Vec<String>)> = facts
        .defs
        .iter()
        .filter(|d| d.kind == DefKind::Function)
        .map(|d| (d.name.as_str(), d.owner.clone()))
        .collect();
    assert_eq!(
        names,
        [
            ("outer", Vec::<String>::new()),
            ("inner", vec!["outer".to_string()]),
        ],
    );
}

#[test]
fn a_function_written_inside_a_conditional_is_still_declared() {
    // Its name is lexical, and this scan measures the text rather than which
    // branch runs. A conditional is not a frame, so the owner chain is empty.
    let facts = extract("lib/util.bash", "if true; then\n  g() { :; }\nfi\n");
    let g = facts
        .defs
        .iter()
        .find(|d| d.kind == DefKind::Function)
        .expect("g");
    assert_eq!(g.name, "g");
    assert!(g.owner.is_empty());
}

#[test]
fn a_plain_path_is_a_literal_however_it_is_quoted() {
    assert_eq!(
        spec("source lib/util.bash\n").as_deref(),
        Some("lib/util.bash")
    );
    assert_eq!(
        spec("source \"lib/util.bash\"\n").as_deref(),
        Some("lib/util.bash")
    );
    assert_eq!(
        spec("source 'lib/util.bash'\n").as_deref(),
        Some("lib/util.bash")
    );
    // One literal written in two pieces.
    assert_eq!(
        spec("source \"lib/\"util.bash\n").as_deref(),
        Some("lib/util.bash")
    );
    assert_eq!(
        spec("source 'lib/''util.bash'\n").as_deref(),
        Some("lib/util.bash")
    );
    // `.` is `source` spelled the POSIX way.
    assert_eq!(spec(". ./util.bash\n").as_deref(), Some("./util.bash"));
}

#[test]
fn anything_the_shell_would_expand_is_not_a_literal() {
    // Never guessed: each of these names a file only the running shell knows.
    for source in [
        "source $lib\n",                            // a bare expansion
        "source \"$dir/util.bash\"\n",              // a composed path
        "source \"${dir}/util.bash\"\n",            // the braced spelling
        "source \"$(dirname \"$0\")/util.bash\"\n", // a command substitution
        "source ~/util.bash\n",                     // tilde expansion
        "source lib/*.bash\n",                      // a glob
        "source lib/{a,b}.bash\n",                  // a brace list
        "source \"lib/a\\$b.bash\"\n",              // a backslash escape
        "source $'lib/util.bash'\n",                // ANSI-C quoting
    ] {
        assert_eq!(spec(source), None, "{source:?} was read as a literal");
    }
}

#[test]
fn an_assignment_prefix_does_not_hide_the_command() {
    assert_eq!(
        spec("BATS_QUIET=1 source lib/util.bash\n").as_deref(),
        Some("lib/util.bash"),
    );
}

#[test]
fn a_source_with_no_argument_is_not_an_import_site() {
    let facts = extract("lib/util.bash", "source\n");
    assert!(facts.refs.is_empty(), "{:?}", facts.refs);
    assert!(facts.header.sources.is_empty());
}

#[test]
fn only_the_two_spellings_of_source_are_read() {
    // Recorded non-claims. `builtin source x` and `command . x` really do
    // source a file; reading the head of a wrapper is a second command model,
    // and nothing in the measured corpus writes one. `load` is a bats
    // function, not shell syntax, and it appears only in `.bats` files this
    // track does not own.
    for source in [
        "builtin source lib/util.bash\n",
        "command . lib/util.bash\n",
        "load test_helper\n",
        "eval source lib/util.bash\n",
    ] {
        let facts = extract("lib/util.bash", source);
        assert!(facts.refs.is_empty(), "{source:?} -> {:?}", facts.refs);
    }
}

#[test]
fn a_reference_names_its_enclosing_function_and_nothing_deeper() {
    let facts = extract(
        "lib/util.bash",
        "source lib/a.bash\nouter() {\n  inner() {\n    source lib/b.bash\n  }\n}\n",
    );
    assert_eq!(facts.refs.len(), 2);
    assert!(facts.refs[0].enclosing.is_none(), "top level names nothing");
    let inner = facts.refs[1].enclosing.as_ref().expect("an encloser");
    assert_eq!(inner.path, ["outer", "inner"]);
    assert_eq!(inner.kind, DefKind::Function);
}

#[test]
fn the_tier_two_contract_holds_on_every_reference() {
    let facts = extract(
        "lib/util.bash",
        "source lib/a.bash\nf() {\n  g \"$x\"\n  local y=1\n  source \"$y\"\n}\n",
    );
    assert_eq!(facts.refs.len(), 2, "{:?}", facts.refs);
    for r in &facts.refs {
        assert_eq!(r.kind, RefKind::Import);
        assert_eq!(r.space, DeclSpace::Namespace);
        // Tier 2 emits no expression-level reference, so nothing here can
        // name a local.
        assert!(!r.locally_bound);
        assert_eq!(r.argc, None);
    }
    assert_eq!(facts.refs[0].target.root, TargetRoot::Name);
    assert_eq!(facts.refs[0].target.segments, ["lib/a.bash"]);
    // A computed specifier's root is not a name.
    assert_eq!(facts.refs[1].target.root, TargetRoot::Expr);
    assert!(facts.refs[1].target.segments.is_empty());
}

#[test]
fn the_raw_target_is_the_site_as_written() {
    let facts = extract("lib/util.bash", "source \"$dir/a.bash\"\n. lib/b.bash\n");
    let raw: Vec<&str> = facts.refs.iter().map(|r| r.raw_target.as_str()).collect();
    assert_eq!(raw, ["source \"$dir/a.bash\"", ". lib/b.bash"]);
}

#[test]
fn every_clause_is_paired_with_exactly_one_reference() {
    // The pairing is by span, so a clause the scope cannot find would
    // silently become `DynamicModuleSpecifier` for a perfectly literal path.
    let facts = extract(
        "lib/util.bash",
        "source lib/a.bash\nf() {\n  source \"$x\"\n}\n. 'lib/b.bash'\n",
    );
    assert_eq!(facts.header.sources.len(), facts.refs.len());
    for (clause, r) in facts.header.sources.iter().zip(&facts.refs) {
        assert_eq!(clause.span, r.span);
    }
}

#[test]
fn records_come_out_in_source_order() {
    let facts = extract(
        "lib/util.bash",
        "source lib/a.bash\nb() { :; }\na() { :; }\nsource lib/c.bash\n",
    );
    let lines: Vec<u32> = facts.refs.iter().map(|r| r.span.line).collect();
    assert_eq!(lines, [1, 4]);
    assert!(
        facts
            .defs
            .windows(2)
            .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
        "{:?}",
        facts.defs,
    );
}
