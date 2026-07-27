//! PHP extraction, one fixture per construct.
//!
//! The tier-2 contract is what these assert: **definitions, structure and
//! imports, and nothing else.** A PHP extractor that emitted calls or type
//! uses would put references in the denominator that no tier-2 resolver
//! links, which is tier-1 coverage claimed without tier-1 work — so the
//! reference kinds this file allows are checked as an invariant, not as a
//! detail of one fixture.

use arthron::model::{DeclSpace, DefKind, Definition, RefKind, Reference, TargetRoot};
use arthron::track_php::extract::extract;

/// `(kind, owner joined by `\`, name)` for every definition, in source order.
fn defs(source: &str) -> Vec<(DefKind, String, String)> {
    extract("src/F.php", source)
        .defs
        .iter()
        .map(|d: &Definition| (d.kind, d.owner.join("\\"), d.name.clone()))
        .collect()
}

/// `(kind, space, raw target, segments joined by `\`)` for every reference.
fn refs(source: &str) -> Vec<(RefKind, DeclSpace, String, String)> {
    extract("src/F.php", source)
        .refs
        .iter()
        .map(|r: &Reference| {
            (
                r.kind,
                r.space,
                r.raw_target.clone(),
                r.target.segments.join("\\"),
            )
        })
        .collect()
}

#[test]
fn a_file_declares_the_namespace_its_definitions_live_in() {
    let facts = extract(
        "src/Client.php",
        "<?php\nnamespace GuzzleHttp;\n\nclass Client {}\n",
    );
    assert_eq!(facts.header.namespaces, ["GuzzleHttp"]);
    assert_eq!(
        defs("<?php\nnamespace GuzzleHttp;\n\nclass Client {}\n"),
        [
            (DefKind::Module, String::new(), "GuzzleHttp".to_string()),
            (
                DefKind::Type,
                "GuzzleHttp".to_string(),
                "Client".to_string()
            ),
        ],
    );
}

#[test]
fn a_file_that_declares_no_namespace_lands_in_the_global_one() {
    let facts = extract("tests/bootstrap-phpstan.php", "<?php\nclass Loose {}\n");
    // One module still, and its name is the empty string: the global
    // namespace is a container with no name, not the absence of one.
    assert_eq!(facts.header.namespaces, [""]);
    assert_eq!(
        defs("<?php\nclass Loose {}\n"),
        [
            (DefKind::Module, String::new(), String::new()),
            (DefKind::Type, String::new(), "Loose".to_string()),
        ],
    );
}

#[test]
fn class_interface_trait_and_enum_are_all_types() {
    let out = defs(concat!(
        "<?php\nnamespace N;\n",
        "class C {}\ninterface I {}\ntrait T {}\nenum E {}\n",
    ));
    assert_eq!(
        out,
        [
            (DefKind::Module, String::new(), "N".to_string()),
            (DefKind::Type, "N".to_string(), "C".to_string()),
            (DefKind::Type, "N".to_string(), "I".to_string()),
            (DefKind::Type, "N".to_string(), "T".to_string()),
            (DefKind::Type, "N".to_string(), "E".to_string()),
        ],
    );
}

#[test]
fn members_are_owned_by_their_namespace_and_their_type() {
    let out = defs(concat!(
        "<?php\nnamespace N;\n",
        "class C {\n",
        "    public const K = 1;\n",
        "    private array $items = [];\n",
        "    public function run(): void {}\n",
        "    public static function make(): self {}\n",
        "}\n",
        "enum E { case One; }\n",
    ));
    assert_eq!(
        out,
        [
            (DefKind::Module, String::new(), "N".to_string()),
            (DefKind::Type, "N".to_string(), "C".to_string()),
            (DefKind::Const, "N\\C".to_string(), "K".to_string()),
            (DefKind::Field, "N\\C".to_string(), "items".to_string()),
            (DefKind::Method, "N\\C".to_string(), "run".to_string()),
            (DefKind::Method, "N\\C".to_string(), "make".to_string()),
            (DefKind::Type, "N".to_string(), "E".to_string()),
            (DefKind::Const, "N\\E".to_string(), "One".to_string()),
        ],
    );
}

#[test]
fn a_promoted_constructor_parameter_is_a_property_of_the_class() {
    let out = defs(concat!(
        "<?php\nnamespace N;\n",
        "class C {\n    public function __construct(private string $x, int $plain) {}\n}\n",
    ));
    assert!(
        out.contains(&(DefKind::Field, "N\\C".to_string(), "x".to_string())),
        "promoted property missing: {out:?}",
    );
    assert!(
        !out.iter().any(|(_, _, name)| name == "plain"),
        "an ordinary parameter is not a property: {out:?}",
    );
}

#[test]
fn a_top_level_function_or_constant_is_owned_by_the_namespace_alone() {
    assert_eq!(
        defs("<?php\nnamespace N;\nconst TOP = 1;\nfunction helper(): void {}\n"),
        [
            (DefKind::Module, String::new(), "N".to_string()),
            (DefKind::Const, "N".to_string(), "TOP".to_string()),
            (DefKind::Function, "N".to_string(), "helper".to_string()),
        ],
    );
}

#[test]
fn one_use_statement_is_one_import_reference() {
    assert_eq!(
        refs("<?php\nnamespace N;\nuse App\\Other\\Thing;\n"),
        [(
            RefKind::Import,
            DeclSpace::Type,
            "App\\Other\\Thing".to_string(),
            "App\\Other\\Thing".to_string(),
        )],
    );
}

#[test]
fn an_alias_does_not_change_what_a_use_names() {
    assert_eq!(
        refs("<?php\nnamespace N;\nuse App\\Thing as T;\n"),
        [(
            RefKind::Import,
            DeclSpace::Type,
            "App\\Thing as T".to_string(),
            "App\\Thing".to_string(),
        )],
    );
}

#[test]
fn a_leading_backslash_names_the_same_thing() {
    // `use \A\B;` and `use A\B;` are the same import — a `use` target is
    // absolute whether or not it is spelled that way.
    assert_eq!(
        refs("<?php\nnamespace N;\nuse \\A\\B;\n"),
        refs("<?php\nnamespace N;\nuse A\\B;\n"),
    );
}

#[test]
fn a_group_use_expands_to_one_reference_per_leaf() {
    assert_eq!(
        refs("<?php\nnamespace N;\nuse App\\Deep\\{Alpha, Beta as B};\n"),
        [
            (
                RefKind::Import,
                DeclSpace::Type,
                "App\\Deep\\Alpha".to_string(),
                "App\\Deep\\Alpha".to_string(),
            ),
            (
                RefKind::Import,
                DeclSpace::Type,
                "App\\Deep\\Beta as B".to_string(),
                "App\\Deep\\Beta".to_string(),
            ),
        ],
    );
}

#[test]
fn a_group_use_reads_the_keyword_each_leaf_writes() {
    // The group form is the one place a single `use` names several of PHP's
    // three symbol tables, and the grammar hangs each keyword on its own
    // clause. A reader that took only the declaration's keyword would call
    // all three of these classes, and `use function A\b;` linking to the
    // *class* `A\b` is the exact wrong edge the sigils exist to prevent.
    assert_eq!(
        refs("<?php\nnamespace N;\nuse A\\{function b, const C, D};\n"),
        [
            (
                RefKind::Import,
                DeclSpace::Value,
                "function A\\b".to_string(),
                "A\\b".to_string(),
            ),
            (
                RefKind::Import,
                DeclSpace::Value,
                "const A\\C".to_string(),
                "A\\C".to_string(),
            ),
            (
                RefKind::Import,
                DeclSpace::Type,
                "A\\D".to_string(),
                "A\\D".to_string(),
            ),
        ],
    );
    // An alias does not hide the keyword either: it is part of the literal
    // text at the site, and `UseKind::of` reads it back off the front.
    assert_eq!(
        refs("<?php\nnamespace N;\nuse A\\{function b as bb};\n"),
        [(
            RefKind::Import,
            DeclSpace::Value,
            "function A\\b as bb".to_string(),
            "A\\b".to_string(),
        )],
    );
}

#[test]
fn a_trailing_comma_in_a_group_use_adds_no_leaf() {
    // Legal PHP since 7.2, and the grammar answers it with a zero-width
    // clause holding an empty `name`. Reading it emits a third reference
    // from a two-leaf import, named `App\` — a name the source never wrote,
    // sitting in the denominator of the rate.
    assert_eq!(
        refs("<?php\nnamespace N;\nuse App\\{Alpha, Beta,};\n"),
        refs("<?php\nnamespace N;\nuse App\\{Alpha, Beta};\n"),
    );
    assert_eq!(
        refs("<?php\nnamespace N;\nuse App\\{Alpha, Beta,};\n").len(),
        2
    );
}

#[test]
fn a_use_the_grammar_could_not_parse_states_nothing() {
    // `use \App\{Alpha};` is valid PHP and tree-sitter-php cannot parse it:
    // it reads the prefix `\App` as a *finished* clause and parks `\{Alpha}`
    // in an `ERROR` sibling. Emitting from that tree drops the import the
    // source wrote and mints one it did not — and `App` alone is a namespace
    // a repository may well declare, so the resolver's rule 2 links the
    // phantom. Nothing is the only honest answer; a guess at the leaf is
    // still a guess.
    assert!(
        refs("<?php\nnamespace N;\nuse \\App\\{Alpha};\n").is_empty(),
        "{:?}",
        refs("<?php\nnamespace N;\nuse \\App\\{Alpha};\n"),
    );
    // The guard is the declaration's own subtree and not the file's: a
    // `use` the grammar *did* parse still states its import beside a class
    // body that does not parse.
    assert_eq!(
        refs("<?php\nnamespace N;\nuse App\\Thing;\nclass C { public function f( "),
        [(
            RefKind::Import,
            DeclSpace::Type,
            "App\\Thing".to_string(),
            "App\\Thing".to_string(),
        )],
    );
}

#[test]
fn a_comma_separated_use_carries_its_keyword_to_every_clause() {
    // tree-sitter hangs the `function` keyword on the first clause alone;
    // PHP applies it to all of them. A reader of the second clause that
    // trusted the tree would look the name up in the class table.
    assert_eq!(
        refs("<?php\nnamespace N;\nuse function A\\b, A\\c;\n"),
        [
            (
                RefKind::Import,
                DeclSpace::Value,
                "function A\\b".to_string(),
                "A\\b".to_string(),
            ),
            (
                RefKind::Import,
                DeclSpace::Value,
                "function A\\c".to_string(),
                "A\\c".to_string(),
            ),
        ],
    );
}

#[test]
fn use_const_names_the_constant_table() {
    assert_eq!(
        refs("<?php\nnamespace N;\nuse const A\\{G, H};\n"),
        [
            (
                RefKind::Import,
                DeclSpace::Value,
                "const A\\G".to_string(),
                "A\\G".to_string(),
            ),
            (
                RefKind::Import,
                DeclSpace::Value,
                "const A\\H".to_string(),
                "A\\H".to_string(),
            ),
        ],
    );
}

#[test]
fn a_trait_use_inside_a_class_body_is_not_an_import() {
    // The provenance's hazard: one keyword, two meanings, told apart by
    // position. Composing a trait imports no name into the file, and a
    // tier-2 track emits no inheritance reference — so it emits nothing.
    let facts = extract(
        "src/C.php",
        "<?php\nnamespace N;\nuse App\\Real;\nclass C {\n    use Helper;\n    use H2, H3;\n}\n",
    );
    assert_eq!(
        facts
            .refs
            .iter()
            .map(|r| r.raw_target.clone())
            .collect::<Vec<_>>(),
        ["App\\Real"],
    );
}

#[test]
fn a_closure_capture_is_not_an_import() {
    // The third meaning of `use`: binding an outer variable into a closure.
    let facts = extract(
        "src/C.php",
        "<?php\nnamespace N;\nfunction f($a) {\n    return function ($q) use ($a) { return $q; };\n}\n",
    );
    assert!(facts.refs.is_empty(), "{:?}", facts.refs);
}

#[test]
fn one_file_can_declare_several_namespaces() {
    // `tests/bootstrap.php`'s shape: the file-to-namespace mapping is not a
    // function, so a definition's namespace is the block it sits in.
    let facts = extract(
        "tests/bootstrap.php",
        concat!(
            "<?php\n",
            "namespace {\n    class Loose {}\n}\n",
            "namespace App\\Test {\n    use App\\Server;\n    class T {}\n}\n",
            "namespace App\\Handler {\n    function shim(): void {}\n}\n",
        ),
    );
    assert_eq!(facts.header.namespaces, ["", "App\\Test", "App\\Handler"]);
    let out: Vec<(DefKind, String, String)> = facts
        .defs
        .iter()
        .map(|d| (d.kind, d.owner.join("\\"), d.name.clone()))
        .collect();
    assert_eq!(
        out,
        [
            (DefKind::Module, String::new(), String::new()),
            (DefKind::Type, String::new(), "Loose".to_string()),
            (DefKind::Module, String::new(), "App\\Test".to_string()),
            (DefKind::Type, "App\\Test".to_string(), "T".to_string()),
            (DefKind::Module, String::new(), "App\\Handler".to_string()),
            (
                DefKind::Function,
                "App\\Handler".to_string(),
                "shim".to_string()
            ),
        ],
    );
}

#[test]
fn an_import_is_enclosed_by_the_namespace_block_it_sits_in() {
    let facts = extract(
        "tests/bootstrap.php",
        concat!(
            "<?php\n",
            "namespace {\n    use A\\One;\n}\n",
            "namespace App\\Test {\n    use A\\Two;\n}\n",
        ),
    );
    let enclosing: Vec<(String, Vec<String>)> = facts
        .refs
        .iter()
        .map(|r| {
            let e = r.enclosing.clone().expect("an import states its namespace");
            assert_eq!(e.kind, DefKind::Module);
            (r.raw_target.clone(), e.path)
        })
        .collect();
    assert_eq!(
        enclosing,
        [
            ("A\\One".to_string(), vec![String::new()]),
            ("A\\Two".to_string(), vec!["App\\Test".to_string()]),
        ],
    );
}

#[test]
fn the_tier_two_contract_holds_no_call_and_no_type_reference() {
    let facts = extract(
        "src/C.php",
        concat!(
            "<?php\nnamespace N;\n",
            "use A\\B;\n",
            "class C extends \\Base implements \\Marker {\n",
            "    public function go(\\Psr\\Log\\LoggerInterface $l): \\Ret {\n",
            "        $x = new \\Other\\Thing();\n",
            "        return helper($x->y());\n",
            "    }\n",
            "}\n",
        ),
    );
    for r in &facts.refs {
        assert_eq!(r.kind, RefKind::Import, "{:?}", r.raw_target);
    }
    assert_eq!(facts.refs.len(), 1);
}

#[test]
fn no_import_is_ever_locally_bound() {
    // LocalBinding does not apply at tier 2: there is no expression-level
    // reference, so nothing a block binds can be named. The flag is a fact
    // the extractor states, and stating it wrongly would move references out
    // of both terms of the rate.
    let facts = extract(
        "src/C.php",
        "<?php\nnamespace N;\nuse A\\B;\nfunction f($B) { return $B; }\n",
    );
    assert!(facts.refs.iter().all(|r| !r.locally_bound));
    assert!(facts.refs.iter().all(|r| r.argc.is_none()));
    assert!(facts.refs.iter().all(|r| r.target.root == TargetRoot::Name));
}

#[test]
fn an_unparseable_file_still_yields_its_container() {
    // tree-sitter is error-tolerant, and a scan that met a broken file with
    // no records at all would report a missing file as a clean one.
    let facts = extract("src/Broken.php", "<?php\nnamespace N;\nclass {{{ \n");
    assert_eq!(facts.header.namespaces, ["N"]);
    assert_eq!(facts.defs.first().map(|d| d.kind), Some(DefKind::Module));
}
