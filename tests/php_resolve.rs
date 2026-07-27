//! PHP's import model, one test per rule.
//!
//! Every reference ends `Resolved`, `External`, or `Unresolved` with a
//! reason. What these hold is *which* reason: the difference between "the map
//! points somewhere and it is empty", "the file is here and the name is not",
//! and "this build never read a map at all" is the whole value of the
//! taxonomy, and collapsing any two of them would hide a different bug.

use std::collections::{BTreeSet, HashSet};

use arthron::lang::{Language, Resolver};
use arthron::model::{Domain, NodeId, node_id};
use arthron::track_php::extract::extract;
use arthron::track_php::lang::PhpLang;
use arthron::track_php::project::PhpProject;
use arthron::track_php::resolve::{PhpResolver, PhpScope};
use arthron::{Outcome, UnresolvedReason};

/// A project with the given PSR-4 prefixes and file set.
fn project(psr4: &[(&str, &str)], files: &[&str]) -> PhpProject {
    PhpProject::new(
        psr4.iter()
            .map(|(p, d)| ((*p).to_string(), vec![(*d).to_string()]))
            .collect(),
        true,
        files
            .iter()
            .map(|f| (*f).to_string())
            .collect::<BTreeSet<_>>(),
    )
}

/// `(raw target, outcome)` for every reference in one file.
fn outcomes(
    cfg: &PhpProject,
    source: &str,
    known: &[&str],
) -> Vec<(String, Outcome<NodeId, String>)> {
    let table: HashSet<NodeId> = known.iter().map(|fqn| node_id(Domain::Php, fqn)).collect();
    let facts = extract("src/F.php", source);
    facts
        .refs
        .iter()
        .map(|r| {
            (
                r.raw_target.clone(),
                PhpResolver.resolve(cfg, &PhpScope, r, &table).outcome,
            )
        })
        .collect()
}

fn reason(cfg: &PhpProject, source: &str, known: &[&str]) -> UnresolvedReason {
    match &outcomes(cfg, source, known)[0].1 {
        Outcome::Unresolved(r) => r.clone(),
        other => panic!("expected an unresolved outcome, got {other:?}"),
    }
}

const GUZZLE: &[(&str, &str)] = &[("GuzzleHttp", "src"), ("GuzzleHttp\\Tests", "tests")];

#[test]
fn a_use_naming_an_in_repository_class_resolves_to_it() {
    let cfg = project(GUZZLE, &["src/Cookie/CookieJar.php"]);
    assert_eq!(
        outcomes(
            &cfg,
            "<?php\nnamespace GuzzleHttp;\nuse GuzzleHttp\\Cookie\\CookieJar;\n",
            &["GuzzleHttp\\Cookie#CookieJar"],
        ),
        [(
            "GuzzleHttp\\Cookie\\CookieJar".to_string(),
            Outcome::Resolved(node_id(Domain::Php, "GuzzleHttp\\Cookie#CookieJar")),
        )],
    );
}

#[test]
fn both_autoload_blocks_reach_their_tree() {
    // The corpus's shape: `GuzzleHttp\` → src/ and `GuzzleHttp\Tests\` →
    // tests/. A resolver reading only the first would place the second
    // prefix's names under src/ and miss every one of them.
    let cfg = project(GUZZLE, &["tests/Server.php"]);
    let out = outcomes(
        &cfg,
        "<?php\nnamespace GuzzleHttp\\Tests;\nuse GuzzleHttp\\Tests\\Server;\n",
        &["GuzzleHttp\\Tests#Server"],
    );
    assert!(matches!(out[0].1, Outcome::Resolved(_)), "{out:?}");
}

#[test]
fn a_use_may_name_a_namespace_rather_than_a_class() {
    // `use GuzzleHttp\Cookie as C;` then `C\Foo` — the container is the node,
    // and the class key misses first.
    let cfg = project(GUZZLE, &[]);
    assert_eq!(
        outcomes(
            &cfg,
            "<?php\nnamespace GuzzleHttp;\nuse GuzzleHttp\\Cookie as C;\n",
            &["GuzzleHttp\\Cookie"],
        ),
        [(
            "GuzzleHttp\\Cookie as C".to_string(),
            Outcome::Resolved(node_id(Domain::Php, "GuzzleHttp\\Cookie")),
        )],
    );
}

#[test]
fn a_class_and_a_namespace_spelled_alike_are_two_nodes() {
    // PHP spells the namespace `A\B` and the class `B` of namespace `A` the
    // same way. The FQN grammar is what keeps them apart, and a probe that
    // found the wrong one would be a wrong edge.
    let cfg = project(&[("A", "src")], &[]);
    let class = outcomes(&cfg, "<?php\nnamespace N;\nuse A\\B;\n", &["A#B"]);
    assert_eq!(
        class[0].1,
        Outcome::Resolved(node_id(Domain::Php, "A#B")),
        "the class key is probed first",
    );
    let namespace = outcomes(&cfg, "<?php\nnamespace N;\nuse A\\B;\n", &["A\\B"]);
    assert_eq!(
        namespace[0].1,
        Outcome::Resolved(node_id(Domain::Php, "A\\B")),
    );
}

#[test]
fn an_undeclared_vendor_namespace_is_external() {
    let cfg = project(GUZZLE, &[]);
    assert_eq!(
        outcomes(
            &cfg,
            "<?php\nnamespace GuzzleHttp;\nuse Psr\\Http\\Message\\RequestInterface;\n",
            &[],
        )[0]
        .1,
        Outcome::External("Psr".to_string()),
    );
}

#[test]
fn a_global_namespace_class_the_repository_does_not_declare_is_external() {
    // `use RuntimeException;` names the runtime's own namespace. One node for
    // all of them rather than a package per class, because there is no
    // package: they are the interpreter.
    let cfg = project(GUZZLE, &[]);
    assert_eq!(
        outcomes(
            &cfg,
            "<?php\nnamespace GuzzleHttp;\nuse RuntimeException;\n",
            &[]
        )[0]
        .1,
        Outcome::External("php:global".to_string()),
    );
}

#[test]
fn a_name_under_a_declared_prefix_whose_file_is_absent_is_module_not_found() {
    // The corpus's largest bucket, and the reason this track's rate is not a
    // fake 1.0: `guzzlehttp/psr7` is a *different composer package* that
    // shares this repository's vendor namespace root, so PSR-4 says the name
    // belongs at `src/Psr7/Request.php` and nothing is there.
    let cfg = project(GUZZLE, &["src/Client.php"]);
    assert_eq!(
        reason(
            &cfg,
            "<?php\nnamespace GuzzleHttp;\nuse GuzzleHttp\\Psr7\\Request;\n",
            &[],
        ),
        UnresolvedReason::ModuleNotFound,
    );
}

#[test]
fn a_sibling_package_is_never_quietly_promoted_to_external() {
    // The one move that would lift this track's rate without linking a single
    // extra reference: `External` sits outside *both* terms.
    let cfg = project(GUZZLE, &[]);
    let out = outcomes(
        &cfg,
        "<?php\nnamespace GuzzleHttp;\nuse GuzzleHttp\\Promise\\PromiseInterface;\n",
        &[],
    );
    assert!(
        !matches!(out[0].1, Outcome::External(_)),
        "a name under this repository's own prefix is not somebody else's: {out:?}",
    );
}

#[test]
fn a_name_whose_file_is_here_and_whose_definition_is_not_is_our_bug() {
    // The bucket reserved for meaning arthron's own error: PSR-4 mapped the
    // name onto a file the walk found, and the symbol table does not hold it.
    let cfg = project(GUZZLE, &["src/Cookie/CookieJar.php"]);
    assert_eq!(
        reason(
            &cfg,
            "<?php\nnamespace GuzzleHttp;\nuse GuzzleHttp\\Cookie\\CookieJar;\n",
            &[],
        ),
        UnresolvedReason::NoMatchingDefinition,
    );
}

#[test]
fn no_psr4_map_at_all_blames_the_layout_and_not_the_name() {
    let cfg = PhpProject::default();
    assert!(!cfg.layout_known());
    assert_eq!(
        reason(
            &cfg,
            "<?php\nnamespace N;\nuse Psr\\Log\\LoggerInterface;\n",
            &[]
        ),
        UnresolvedReason::ProjectLayoutUnknown,
    );
    // A definition that *is* indexed still resolves: an unknown layout costs
    // nothing where the answer needs no map.
    let out = outcomes(&cfg, "<?php\nnamespace N;\nuse A\\B;\n", &["A#B"]);
    assert!(matches!(out[0].1, Outcome::Resolved(_)), "{out:?}");
}

#[test]
fn a_prefix_claims_the_subtree_under_it_and_never_itself() {
    let cfg = project(&[("GuzzleHttp", "src")], &[]);
    // `use GuzzleHttp;` names a one-segment global class, which the prefix
    // `GuzzleHttp\` does not claim.
    assert_eq!(
        outcomes(&cfg, "<?php\nnamespace N;\nuse GuzzleHttp;\n", &[])[0].1,
        Outcome::External("php:global".to_string()),
    );
}

#[test]
fn use_function_and_use_const_read_their_own_symbol_tables() {
    let cfg = project(&[("A", "src")], &[]);
    // The three tables are three keys: a class `A\b`, a function `A\b()` and
    // a constant `A\b!` are three nodes, and a `use function` that landed on
    // the class would be a wrong edge.
    let out = outcomes(
        &cfg,
        "<?php\nnamespace N;\nuse function A\\b;\nuse const A\\C;\nuse A\\b;\n",
        &["A#b()", "A#C!", "A#b"],
    );
    assert_eq!(
        out.iter().map(|(raw, _)| raw.clone()).collect::<Vec<_>>(),
        ["function A\\b", "const A\\C", "A\\b"],
    );
    assert_eq!(out[0].1, Outcome::Resolved(node_id(Domain::Php, "A#b()")));
    assert_eq!(out[1].1, Outcome::Resolved(node_id(Domain::Php, "A#C!")));
    assert_eq!(out[2].1, Outcome::Resolved(node_id(Domain::Php, "A#b")));
}

#[test]
fn a_use_function_miss_under_a_claimed_prefix_is_module_not_found() {
    // PSR-4 maps class names onto files and says nothing about functions, so
    // there is no path to test — the `files` autoload entry is what loads
    // those, and the corpus declares none.
    let cfg = project(&[("A", "src")], &["src/b.php"]);
    assert_eq!(
        reason(&cfg, "<?php\nnamespace N;\nuse function A\\b;\n", &[]),
        UnresolvedReason::ModuleNotFound,
    );
}

#[test]
fn the_config_digest_covers_every_input_a_resolution_reads() {
    // Both of them, because rule 4 reads both. The manifest decides which
    // names this repository claims; the file set decides whether the path
    // PSR-4 maps a claimed name onto is here, which is the whole difference
    // between `NoMatchingDefinition` and `ModuleNotFound`. An input a
    // resolution reads and this does not cover is one an incremental scan
    // cannot invalidate on, and a warm store that keeps the old answer.
    let base = project(&[("App", "src")], &["src/Client.php"]);
    let same = project(&[("App", "src")], &["src/Client.php"]);
    assert_eq!(
        PhpResolver.config_digest(&base),
        PhpResolver.config_digest(&same),
    );
    let remapped = project(&[("App", "lib")], &["src/Client.php"]);
    assert_ne!(
        PhpResolver.config_digest(&base),
        PhpResolver.config_digest(&remapped),
        "the manifest moved",
    );
    let grown = project(
        &[("App", "src")],
        &["src/Client.php", "src/Missing/Thing.php"],
    );
    assert_ne!(
        PhpResolver.config_digest(&base),
        PhpResolver.config_digest(&grown),
        "a file appeared at a path rule 4 tests",
    );
    // Bounded: the file set is hashed, not concatenated, so the fingerprint
    // is the same size on a repository of any size.
    assert_eq!(
        PhpResolver.config_digest(&base).len(),
        PhpResolver.config_digest(&grown).len(),
    );
}

#[test]
fn every_probe_is_recorded_and_nothing_else_is() {
    // The candidate list feeds the invalidation index: a probe missing from
    // it is a reference an incremental scan never wakes.
    let cfg = project(GUZZLE, &[]);
    let facts = extract("src/F.php", "<?php\nnamespace N;\nuse A\\B;\n");
    let table: HashSet<NodeId> = HashSet::new();
    let res = PhpResolver.resolve(&cfg, &PhpScope, &facts.refs[0], &table);
    assert_eq!(
        res.candidates,
        [node_id(Domain::Php, "A#B"), node_id(Domain::Php, "A\\B"),],
        "the class key, then the namespace key",
    );

    let hit: HashSet<NodeId> = [node_id(Domain::Php, "A#B")].into_iter().collect();
    let res = PhpResolver.resolve(&cfg, &PhpScope, &facts.refs[0], &hit);
    assert_eq!(
        res.candidates,
        [node_id(Domain::Php, "A#B")],
        "a resolution that stopped at the first key probed only that one",
    );
}

#[test]
fn the_resolver_declares_no_link_kind_and_needs_no_scope() {
    assert!(PhpResolver.link_kinds().is_empty());
    assert_eq!(PhpLang::LANG.domain(), PhpLang::DOMAIN);
    // The scope carries nothing, and that is a claim rather than an
    // oversight: a `use` names an absolute name, so there is no file-local
    // environment to read it against. Two unrelated files build the same one.
    let cfg = project(GUZZLE, &[]);
    let table: HashSet<NodeId> = HashSet::new();
    let a = extract("src/A.php", "<?php\nnamespace A;\nuse X\\Y;\n");
    let b = extract("src/B.php", "<?php\nnamespace B;\nclass B {}\n");
    assert_eq!(
        PhpResolver.scope(&cfg, &a, &table),
        PhpResolver.scope(&cfg, &b, &table),
    );
}

#[test]
fn a_reference_kind_this_tier_does_not_link_says_so() {
    // Structurally unreachable through the extractor — it emits one kind —
    // and still answered, because `resolve` is total over `Reference`.
    use arthron::model::{DeclSpace, RefKind, RefTarget, Span, TargetRoot};
    let call = arthron::model::Reference {
        kind: RefKind::Call,
        space: DeclSpace::Value,
        raw_target: "helper".to_string(),
        target: RefTarget {
            root: TargetRoot::Name,
            segments: vec!["helper".to_string()],
        },
        locally_bound: false,
        argc: Some(0),
        enclosing: None,
        span: Span {
            byte_start: 0,
            byte_end: 6,
            line: 1,
        },
    };
    let cfg = project(GUZZLE, &[]);
    let table: HashSet<NodeId> = HashSet::new();
    assert_eq!(
        PhpResolver.resolve(&cfg, &PhpScope, &call, &table).outcome,
        Outcome::Unresolved(UnresolvedReason::TierTwoLanguage),
    );
}
