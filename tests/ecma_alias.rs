//! Alias chains through EcmaScript barrels.
//!
//! A re-export is an alias: an index key that names a node under another
//! name. Before the store surfaced alias entries, a chain stopped one hop
//! short — an import of a barrel resolved to the barrel's *alias* node rather
//! than to the definition it forwards to, and a name arriving through
//! `export *` was reported `WildcardImport` because the star's name set was a
//! fixed point nothing walked.
//!
//! These fixtures pin the three shapes that walk has to get right: a named
//! chain of more than one hop, a star chain, and a cycle — which must be an
//! honest `AliasCycle`, never a hang and never a silent drop.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use arthron::model::reason_name;
use arthron::store::{Store, StoredOutcome};
use arthron::track_ecma::scan_ecma;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn rows(db: &Path) -> BTreeMap<(String, String, String), Vec<String>> {
    let store = Store::open(db).expect("store opens");
    let snapshot = store.snapshot().expect("snapshot");
    let mut out: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    for (key, record) in &snapshot.rows {
        let rendered = match &record.outcome {
            StoredOutcome::Resolved(id) => match store.node(id).expect("node read") {
                Some(arthron::store::NodeRecord::Definition { fqn, .. }) => {
                    format!("resolved {fqn}")
                }
                Some(arthron::store::NodeRecord::Package { import_path, .. }) => {
                    format!("resolved {import_path}")
                }
                Some(arthron::store::NodeRecord::External { package, .. }) => {
                    format!("resolved external:{package}")
                }
                None => "resolved <dangling>".to_string(),
            },
            StoredOutcome::External(package) => format!("external {package}"),
            StoredOutcome::Unresolved(code) => format!("unresolved {}", reason_name(*code)),
        };
        out.entry((
            key.file.clone(),
            key.raw_target.clone(),
            key.enclosing.clone(),
        ))
        .or_default()
        .push(rendered);
    }
    out
}

fn row<'a>(
    table: &'a BTreeMap<(String, String, String), Vec<String>>,
    file: &str,
    target: &str,
    enclosing: &str,
) -> &'a str {
    let key = (file.to_string(), target.to_string(), enclosing.to_string());
    let found = match table.get(&key) {
        Some(v) => v,
        None => panic!("no row for {key:?}\nrows: {table:#?}"),
    };
    assert_eq!(
        found.len(),
        1,
        "expected one outcome for {key:?}, found {found:?}",
    );
    found[0].as_str()
}

fn pkg(root: &Path) {
    write(root, "package.json", r#"{"name":"app","type":"module"}"#);
}

/// Two hops of named re-export: `main` names `parse`, which `index` re-exports
/// from `mid`, which re-exports it from `impl`. The edge must land on the
/// definition, not on either intermediate alias.
#[test]
fn a_two_hop_barrel_chain_reaches_the_definition() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    pkg(root);
    write(
        root,
        "src/impl.ts",
        "export function parse(s: string) { return s }\n",
    );
    write(root, "src/mid.ts", "export { parse } from './impl.js'\n");
    write(root, "src/index.ts", "export { parse } from './mid.js'\n");
    write(
        root,
        "src/main.ts",
        "import { parse } from './index.js'\nexport function run() { return parse('x') }\n",
    );

    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "src/main.ts", "parse", "src/main.ts#value:run"),
        "resolved src/impl.ts#value:parse",
        "two hops of alias must reach the definition itself",
    );
}

/// A renamed re-export changes the name but not the destination.
#[test]
fn a_renamed_re_export_reaches_the_definition_it_renames() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    pkg(root);
    write(
        root,
        "src/impl.ts",
        "export function parse(s: string) { return s }\n",
    );
    write(
        root,
        "src/index.ts",
        "export { parse as parseInput } from './impl.js'\n",
    );
    write(
        root,
        "src/main.ts",
        "import { parseInput } from './index.js'\nexport function run() { return parseInput('x') }\n",
    );

    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "src/main.ts", "parseInput", "src/main.ts#value:run"),
        "resolved src/impl.ts#value:parse",
    );
}

/// `export *` is a chain too. Before alias entries this was `WildcardImport`
/// on every name; the star's targets are known, so the walk can run.
#[test]
fn a_star_re_export_resolves_the_name_it_forwards() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    pkg(root);
    write(
        root,
        "src/impl.ts",
        "export function parse(s: string) { return s }\n",
    );
    write(
        root,
        "src/other.ts",
        "export function stringify() { return '' }\n",
    );
    write(
        root,
        "src/index.ts",
        "export * from './impl.js'\nexport * from './other.js'\n",
    );
    write(
        root,
        "src/main.ts",
        "import { parse, stringify } from './index.js'\n\
         export function run() { return parse(stringify()) }\n",
    );

    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "src/main.ts", "parse", "src/main.ts#value:run"),
        "resolved src/impl.ts#value:parse",
        "a name arriving through `export *` resolves to its definition",
    );
    assert_eq!(
        row(&table, "src/main.ts", "stringify", "src/main.ts#value:run"),
        "resolved src/other.ts#value:stringify",
        "the walk tries every star target, not just the first",
    );
}

/// A star chain more than one module deep — the vue-core shape, where `vue`
/// stars `runtime-dom`, which stars `runtime-core`.
#[test]
fn a_star_chain_walks_more_than_one_module_deep() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    pkg(root);
    write(root, "src/core.ts", "export function render() {}\n");
    write(root, "src/dom.ts", "export * from './core.js'\n");
    write(root, "src/index.ts", "export * from './dom.js'\n");
    write(
        root,
        "src/main.ts",
        "import { render } from './index.js'\nexport function run() { return render() }\n",
    );

    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "src/main.ts", "render", "src/main.ts#value:run"),
        "resolved src/core.ts#value:render",
    );
}

/// A name no module in the star chain exports is `NoMatchingDefinition` — the
/// walk *completed*, so `WildcardImport` would now be a lie.
#[test]
fn a_name_absent_from_the_whole_star_chain_is_no_matching_definition() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    pkg(root);
    write(
        root,
        "src/impl.ts",
        "export function parse(s: string) { return s }\n",
    );
    write(root, "src/index.ts", "export * from './impl.js'\n");
    write(
        root,
        "src/main.ts",
        "import { missing } from './index.js'\nexport function run() { return missing() }\n",
    );

    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "src/main.ts", "missing", "src/main.ts#value:run"),
        "unresolved NoMatchingDefinition",
    );
}

/// Two star exports supplying one name from different modules is the genuine
/// ambiguity `AmbiguousExport` is for. It survives the walk.
#[test]
fn two_stars_supplying_one_name_stay_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    pkg(root);
    write(root, "src/a.ts", "export function dup() {}\n");
    write(root, "src/b.ts", "export function dup() {}\n");
    write(
        root,
        "src/index.ts",
        "export * from './a.js'\nexport * from './b.js'\n",
    );
    write(
        root,
        "src/main.ts",
        "import { dup } from './index.js'\nexport function run() { return dup() }\n",
    );

    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "src/main.ts", "dup", "src/main.ts#value:run"),
        "unresolved AmbiguousExport",
    );
}

/// A cycle of named re-exports terminates with a reason of its own. The point
/// is that the scan *finishes*: a hang would be a worse failure than a miss.
#[test]
fn an_alias_cycle_is_reported_not_hung() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    pkg(root);
    write(root, "src/a.ts", "export { x } from './b.js'\n");
    write(root, "src/b.ts", "export { x } from './a.js'\n");
    write(
        root,
        "src/main.ts",
        "import { x } from './a.js'\nexport function run() { return x() }\n",
    );

    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "src/main.ts", "x", "src/main.ts#value:run"),
        "unresolved AliasCycle",
    );
}

/// A cycle of star exports is the same obligation on the other shape.
#[test]
fn a_star_cycle_is_reported_not_hung() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    pkg(root);
    write(root, "src/a.ts", "export * from './b.js'\n");
    write(root, "src/b.ts", "export * from './a.js'\n");
    write(
        root,
        "src/main.ts",
        "import { x } from './a.js'\nexport function run() { return x() }\n",
    );

    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    let got = row(&table, "src/main.ts", "x", "src/main.ts#value:run");
    assert!(
        got == "unresolved AliasCycle" || got == "unresolved NoMatchingDefinition",
        "a star cycle must terminate with an honest reason, got {got:?}",
    );
}

/// A barrel that stars a dependency alongside a local module cannot list the
/// dependency's exports. A name the *local* module does not carry is
/// therefore not "no module exports this" — the walk enumerated only part of
/// the set, and the reason has to say so.
///
/// The star entry that forwards outside the repository used to contribute
/// nothing at all, which left the alias node looking like a fully enumerable
/// one-target star and turned a half-finished search into a confident
/// `NoMatchingDefinition`.
#[test]
fn a_star_from_a_dependency_leaves_the_name_set_un_enumerable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    pkg(root);
    write(
        root,
        "src/local.ts",
        "export function parse(s: string) { return s }\n",
    );
    write(
        root,
        "src/index.ts",
        "export * from './local.js'\nexport * from 'dependency'\n",
    );
    write(
        root,
        "src/main.ts",
        "import { stringify } from './index.js'\n\
         export function run() { return stringify() }\n",
    );

    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "src/main.ts", "stringify", "src/main.ts#value:run"),
        "unresolved WildcardImport",
        "a dependency star is a part of the export set this build cannot list",
    );
}

/// The local half of the same barrel still resolves. Recording the
/// dependency star as un-enumerable must not cost the names that *are*
/// knowable: a program where both stars carried `parse` would not compile,
/// so the local hit is the only answer a compiling corpus can mean.
#[test]
fn a_dependency_star_does_not_cost_the_local_stars_answer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    pkg(root);
    write(
        root,
        "src/local.ts",
        "export function parse(s: string) { return s }\n",
    );
    write(
        root,
        "src/index.ts",
        "export * from './local.js'\nexport * from 'dependency'\n",
    );
    write(
        root,
        "src/main.ts",
        "import { parse } from './index.js'\nexport function run() { return parse('x') }\n",
    );

    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "src/main.ts", "parse", "src/main.ts#value:run"),
        "resolved src/local.ts#value:parse",
    );
}
