//! The EcmaScript track end to end: a small fixture tree resolves its
//! cross-file references, and the case studies' canonical *unresolvable*
//! cases land on the right reason.
//!
//! The second half is the point. A resolution rate proves nothing on its own —
//! silently dropping what cannot be linked would raise it — so every fixture
//! below that cannot resolve is asserted to carry the reason its case study
//! names, and the reasons that are meant to be a large permanent floor
//! (`NeedsTypeInference`, `NeedsExpressionType`, `NeedsReceiverType`) are
//! asserted to *be* there rather than quietly reclassified into `LocalBinding`
//! or `External`, which sit outside both terms of the rate.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use arthron::model::{Lang, reason_name};
use arthron::store::{Store, StoredOutcome};
use arthron::track_ecma::scan_ecma;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Every stored row, keyed `(file, raw_target, enclosing)`, with its outcome
/// rendered as a short string a test can read by eye.
fn rows(db: &Path) -> BTreeMap<(String, String, String), Vec<String>> {
    let store = Store::open(db).expect("store opens");
    let snapshot = store.snapshot().expect("snapshot");
    let mut out = BTreeMap::new();
    for (key, record) in &snapshot.rows {
        let rendered = match &record.outcome {
            StoredOutcome::Resolved(id) => match store.node(id).expect("node read") {
                Some(node) => format!("resolved {}", node_name(&node)),
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
        .or_insert_with(Vec::new)
        .push(rendered);
    }
    out
}

fn node_name(node: &arthron::store::NodeRecord) -> String {
    match node {
        arthron::store::NodeRecord::Definition { fqn, .. } => fqn.clone(),
        arthron::store::NodeRecord::Package { import_path, .. } => import_path.clone(),
        arthron::store::NodeRecord::External { package, .. } => format!("external:{package}"),
    }
}

/// Look one row up, panicking with the whole table when it is absent — a
/// missing row is a dropped reference, which is the one failure this project
/// exists to make impossible.
fn row<'a>(
    table: &'a BTreeMap<(String, String, String), Vec<String>>,
    file: &str,
    target: &str,
    enclosing: &str,
) -> &'a str {
    let found = outcomes(table, file, target, enclosing);
    assert_eq!(
        found.len(),
        1,
        "expected one outcome for ({file}, {target}, {enclosing}), found {found:?}",
    );
    found[0]
}

/// Every outcome stored under one `(file, target, enclosing)` key.
///
/// More than one is not a duplicate: the row key also carries the declaration
/// space, and `Foo` in type position and `Foo` in value position are
/// legitimately different symbols with different outcomes (C1).
fn outcomes<'a>(
    table: &'a BTreeMap<(String, String, String), Vec<String>>,
    file: &str,
    target: &str,
    enclosing: &str,
) -> Vec<&'a str> {
    let key = (file.to_string(), target.to_string(), enclosing.to_string());
    match table.get(&key) {
        Some(v) => v.iter().map(String::as_str).collect(),
        None => panic!("no row for {key:?}\nrows: {table:#?}"),
    }
}

/// A TypeScript workspace exercising the cases the case studies call
/// load-bearing: `paths`, barrels, renamed and default exports, `this`/`super`,
/// declaration spaces, and the honest failures.
fn typescript_tree(root: &Path) {
    write(
        root,
        "package.json",
        r#"{"name":"app","type":"module","dependencies":{"lodash":"^4"}}"#,
    );
    // A10: the vue-core case — `@app/*` mapped onto workspace source.
    write(
        root,
        "tsconfig.json",
        r#"{
            // paths are compile-time only; they never rewrite emitted specifiers
            "compilerOptions": {
                "baseUrl": ".",
                "paths": { "@app/*": ["packages/*/src"] },
            },
        }"#,
    );

    write(
        root,
        "packages/core/src/parse.ts",
        concat!(
            "export function parse(input: string): number { return input.length }\n",
            "export function unused(): void {}\n",
        ),
    );
    // B3: the declaration keeps its own name; the entry is called `default`.
    write(
        root,
        "packages/core/src/format.ts",
        "export default function reallyFormat(n: number): string { return String(n) }\n",
    );
    write(
        root,
        "packages/core/src/shapes.ts",
        concat!(
            "export interface Shape { area(): number }\n",
            "export class Base implements Shape {\n",
            "  area(): number { return 0 }\n",
            "  describe(): string { return String(this.area()) }\n",
            "}\n",
        ),
    );
    // B2/B7/B10: a barrel — a rename, an indirect re-export, a bare star.
    write(
        root,
        "packages/core/src/index.ts",
        concat!(
            "export { parse as parseInput } from './parse.js';\n",
            "export { default as format } from './format.js';\n",
            "export * from './shapes.js';\n",
        ),
    );

    write(
        root,
        "packages/app/src/main.ts",
        concat!(
            "import { parseInput, format } from '@app/core';\n",
            "import { parse } from '../../core/src/parse.js';\n",
            "import * as core from '@app/core';\n",
            "import { Base } from '../../core/src/shapes.js';\n",
            "import merge from 'lodash';\n",
            "import { ghost } from './missing.js';\n",
            "\n",
            "export class Derived extends Base {\n",
            "  area(): number { return super.area() + 1 }\n",
            "  run(handler: unknown): void {\n",
            "    parseInput('x');\n",
            "    format(1);\n",
            "    parse('y');\n",
            "    core.parseInput('z');\n",
            "    core.nothingHere();\n",
            "    ghost();\n",
            "    merge({}, {});\n",
            "    this.area();\n",
            "    this.notDeclaredAnywhere();\n",
            "    console.log('hi');\n",
            "    (handler as { go(): void }).go();\n",
            "    const shadow = (parse: (s: string) => number) => parse('inner');\n",
            "    shadow('q');\n",
            "  }\n",
            "}\n",
        ),
    );
}

#[test]
fn a_typescript_workspace_resolves_across_files_and_reports_honest_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    typescript_tree(root);
    let db = root.join("graph.redb");
    let report = scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    let main = "packages/app/src/main.ts";
    // E3: an instance member lives on `C.prototype`, so that is where the
    // edge out of it starts.
    let run = "packages/app/src/main.ts#value:Derived.prototype.run";

    // --- A10: the alias resolves to workspace *source*, not to a guess.
    assert_eq!(
        row(&table, main, "@app/core", main),
        "resolved packages/core/src/index.ts",
    );
    assert_eq!(
        row(&table, main, "../../core/src/parse.js", main),
        "resolved packages/core/src/parse.ts",
        "A3: a specifier carrying the *output* extension names the source",
    );

    // --- B2/B10: a renamed export through a barrel. `parseInput` is a name
    // no file declares; it exists because `index.ts` exports it.
    assert_eq!(
        row(&table, main, "parseInput", run),
        "resolved packages/core/src/index.ts#value:parseInput",
    );
    // --- B3/F2: the local name of a default import is unrelated to the
    // definition's name, so the binding table is the only way back.
    assert_eq!(
        row(&table, main, "format", run),
        "resolved packages/core/src/index.ts#value:format",
    );
    // --- B1: a direct named import needs no alias.
    assert!(
        outcomes(&table, main, "parse", run)
            .contains(&"resolved packages/core/src/parse.ts#value:parse"),
    );
    // --- F4: a namespace import, then a name through its export map.
    assert_eq!(
        row(&table, main, "core.parseInput", run),
        "resolved packages/core/src/index.ts#value:parseInput",
    );

    // --- The barrel's own edges: each alias reaches the terminal definition.
    let barrel = "packages/core/src/index.ts";
    assert_eq!(
        row(
            &table,
            barrel,
            "./parse.js",
            "packages/core/src/index.ts#value:parseInput"
        ),
        "resolved packages/core/src/parse.ts#value:parse",
    );
    assert_eq!(
        row(
            &table,
            barrel,
            "./format.js",
            "packages/core/src/index.ts#value:format"
        ),
        "resolved packages/core/src/format.ts#value:default",
    );

    // --- F6: `this.m()` against the lexically enclosing class. A decision,
    // taken narrowly: the member is declared on the class the reference is
    // inside.
    assert_eq!(
        row(&table, main, "this.area", run),
        "resolved packages/app/src/main.ts#value:Derived.prototype.area",
    );
    // --- F7: `super.m()` through the written heritage, into another file.
    assert_eq!(
        row(
            &table,
            main,
            "super.area",
            "packages/app/src/main.ts#value:Derived.prototype.area"
        ),
        "resolved packages/core/src/shapes.ts#value:Base.prototype.area",
    );

    // --- Externals: a declared dependency and a host global. Neither is a
    // resolution failure, and neither counts toward the rate.
    assert_eq!(row(&table, main, "lodash", main), "external npm:lodash");
    assert_eq!(row(&table, main, "merge", run), "external npm:lodash");
    assert_eq!(row(&table, main, "console.log", run), "external web:global");

    // --- The honest failures, one per case study line.
    //
    // B5: `index.ts` carries a bare `export *`, so its export set is a fixed
    // point over the module graph that this build does not compute. Saying
    // `NoMatchingDefinition` would claim the lookup table was complete.
    assert_eq!(
        row(&table, main, "core.nothingHere", run),
        "unresolved WildcardImport",
    );
    // A1–A14: the specifier is a literal and resolved to no file.
    assert_eq!(
        row(&table, main, "./missing.js", main),
        "unresolved ModuleNotFound",
    );
    assert_eq!(
        row(&table, main, "ghost", run),
        "unresolved ModuleNotFound",
        "a name taken from a module that does not exist reports the module",
    );
    // F6's other half: the receiver type is known, the member is not in it,
    // and a supertype was written — so it is where the member would be.
    assert_eq!(
        row(&table, main, "this.notDeclaredAnywhere", run),
        "unresolved UnindexedSupertype",
    );
    // F5: the operand is an expression, not a name.
    assert_eq!(
        row(&table, main, "(handler as { go(): void }).go", run),
        "unresolved NeedsExpressionType",
    );
    // D5: the arrow's parameter shadows the import. Resolving it would be a
    // *wrong edge*, which is strictly worse than an unresolved reference.
    // F16 in one assertion: `parse()` at method level resolves to the import
    // and `parse('inner')` inside an arrow that binds `parse` does not. They
    // share file, kind, space, encloser *and* text, so a row key without
    // `locally_bound` would collapse them and store one outcome for both.
    let both: Vec<&str> = outcomes(&table, main, "parse", run);
    assert!(
        both.contains(&"resolved packages/core/src/parse.ts#value:parse"),
        "{both:?}",
    );
    assert!(
        both.contains(&"unresolved LocalBinding"),
        "the arrow's own parameter shadows the import: {both:?}",
    );

    // --- Two rates, never one.
    let js = report.per_lang.get(&Lang::JavaScript.code());
    let ts = report.per_lang.get(&Lang::TypeScript.code());
    assert!(ts.is_some(), "TypeScript reports a line");
    assert!(
        js.is_none_or(|t| t.resolved == 0 && t.unresolved_total() == 0),
        "this tree holds no JavaScript, so JavaScript has nothing to report",
    );
    let ts = ts.unwrap();
    assert!(ts.resolved > 0 && ts.unresolved_total() > 0);
}

/// A CommonJS tree: the `require` binding shapes, `module.exports`, index
/// resolution, and the reasons CommonJS makes unavoidable.
fn commonjs_tree(root: &Path) {
    write(root, "package.json", r#"{"name":"cjs-app"}"#);
    write(
        root,
        "lib/util/index.js",
        concat!(
            "function parse(s) { return s.length }\n",
            "function format(n) { return String(n) }\n",
            "module.exports = { parse, format };\n",
        ),
    );
    write(
        root,
        "lib/parser.js",
        concat!(
            "function Parser() {}\n",
            "Parser.prototype.run = function () { return 1 };\n",
            "module.exports = Parser;\n",
        ),
    );
    write(
        root,
        "index.js",
        concat!(
            "const util = require('./lib/util');\n",
            "const { parse } = require('./lib/util');\n",
            "const Parser = require('./lib/parser.js');\n",
            "const fs = require('fs');\n",
            "const plugin = require(process.env.PLUGIN);\n",
            "\n",
            "function main(opts) {\n",
            "  util.format(1);\n",
            "  parse('x');\n",
            "  new Parser();\n",
            "  fs.readFileSync('x');\n",
            "  util.notThere();\n",
            "  opts.go();\n",
            "}\n",
            "module.exports = main;\n",
        ),
    );
}

#[test]
fn a_commonjs_tree_resolves_its_four_require_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    commonjs_tree(root);
    let db = root.join("graph.redb");
    let report = scan_ecma(root, &db).expect("scan");
    let table = rows(&db);
    let main = "index.js#value:main";

    // A4: `LOAD_AS_DIRECTORY` — `./lib/util` is a directory with an index.
    assert_eq!(
        row(&table, "index.js", "./lib/util", "index.js"),
        "resolved lib/util/index.js",
    );
    // C2 shape 1: the whole `module.exports` object, then a member of it.
    assert_eq!(
        row(&table, "index.js", "util.format", main),
        "resolved lib/util/index.js#value:format",
    );
    // C2 shape 2: destructuring binds one exported name.
    assert_eq!(
        row(&table, "index.js", "parse", main),
        "resolved lib/util/index.js#value:parse",
    );
    // C3/F8: `module.exports = Parser`, constructed with `new`.
    assert_eq!(
        row(&table, "index.js", "Parser", main),
        "resolved lib/parser.js#value:default",
    );
    // A13: a builtin is `External`, never `Unresolved`.
    assert_eq!(
        row(&table, "index.js", "fs", "index.js#value:fs"),
        "external node:fs"
    );
    assert_eq!(
        row(&table, "index.js", "fs.readFileSync", main),
        "external node:fs",
    );
    // C8: the specifier is an arbitrary expression. Folding this into
    // `DynamicDispatch` would conflate "cannot find the module" with "cannot
    // find the method", which are different work items.
    assert_eq!(
        row(
            &table,
            "index.js",
            "process.env.PLUGIN",
            "index.js#value:plugin"
        ),
        "unresolved DynamicModuleSpecifier",
    );
    // The module's export map was computed and the name is not in it.
    assert_eq!(
        row(&table, "index.js", "util.notThere", main),
        "unresolved NoMatchingDefinition",
    );
    // The dominant, permanent floor: a method on an untyped parameter. It is
    // supposed to be here, and it must not be gamed away.
    assert_eq!(
        row(&table, "index.js", "opts.go", main),
        "unresolved LocalBinding",
        "`opts` is the function's own parameter, so it is not a node at all",
    );

    let js = &report.per_lang[&Lang::JavaScript.code()];
    assert!(js.resolved > 0);
    assert!(
        !report.per_lang.contains_key(&Lang::TypeScript.code())
            || report.per_lang[&Lang::TypeScript.code()].resolved == 0,
    );
}

#[test]
fn a_javascript_esm_file_does_not_probe_extensions() {
    // A5, and it is non-negotiable: NODE `ESM_RESOLVE` performs URL
    // resolution only, so `./util` resolves to `./util` exactly or fails.
    // Applying the CommonJS probe list here would invent an edge Node would
    // not create — and a wrong edge is worse than a miss, because a miss is
    // counted.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"esm","type":"module"}"#);
    write(root, "util.js", "export function parse() {}\n");
    write(root, "a.js", "import { parse } from './util';\nparse();\n");
    write(
        root,
        "b.js",
        "import { parse } from './util.js';\nparse();\n",
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "a.js", "./util", "a.js"),
        "unresolved ModuleNotFound"
    );
    assert_eq!(row(&table, "b.js", "./util.js", "b.js"), "resolved util.js");
    assert_eq!(
        row(&table, "b.js", "parse", "b.js"),
        "resolved util.js#value:parse",
    );
}

#[test]
fn a_typescript_file_may_resolve_into_a_javascript_one() {
    // The reason the two languages share one `Domain`: a `.ts` file naming a
    // definition in a `.js` file has to probe an identity that can exist.
    // They still report two rates.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"mixed"}"#);
    write(root, "legacy.js", "export function helper() {}\n");
    write(
        root,
        "app.ts",
        "import { helper } from './legacy.js';\nexport function go(): void { helper() }\n",
    );
    let db = root.join("graph.redb");
    let report = scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "app.ts", "./legacy.js", "app.ts"),
        "resolved legacy.js"
    );
    assert_eq!(
        row(&table, "app.ts", "helper", "app.ts#value:go"),
        "resolved legacy.js#value:helper",
    );

    // Two lines, and no way to spell a combined one. `legacy.js` makes no
    // reference at all, so JavaScript has nothing to report — which is a
    // different fact from "measured, found nothing" and is why the driver
    // does not invent a zero row for it.
    let ts = &report.per_lang[&Lang::TypeScript.code()];
    assert!(
        ts.resolved >= 2,
        "the TypeScript file linked into JavaScript"
    );
    assert!(
        report
            .per_lang
            .get(&Lang::JavaScript.code())
            .is_none_or(|t| t.resolved == 0),
    );
    assert_ne!(Lang::JavaScript.code(), Lang::TypeScript.code());
}

#[test]
fn declaration_spaces_keep_a_type_and_a_value_of_one_name_apart() {
    // C1: `interface Foo {}` beside `const Foo = 1` is legal TypeScript and is
    // two symbols. Without the space in the FQN they share a `NodeId` and one
    // silently overwrites the other in the node table.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"spaces"}"#);
    write(
        root,
        "m.ts",
        concat!(
            "export interface Foo { n: number }\n",
            "export function Foo(): number { return 1 }\n",
        ),
    );
    write(
        root,
        "use.ts",
        concat!(
            "import { Foo } from './m.js';\n",
            "export function take(f: Foo): number { return Foo() }\n",
        ),
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    // The same written name, two rows, two different targets — because the
    // reference's *space* is a separate axis from its kind.
    let found = outcomes(&table, "use.ts", "Foo", "use.ts#value:take");
    assert!(
        found.contains(&"resolved m.ts#type:Foo"),
        "the type position reads the Type space: {found:?}",
    );
    assert!(
        found.contains(&"resolved m.ts#value:Foo"),
        "the value position reads the Value space: {found:?}",
    );
    assert_eq!(found.len(), 2, "one written name, two symbols: {found:?}");
}

#[test]
fn every_reference_in_a_fixture_tree_has_exactly_one_stored_outcome() {
    // "The resolver never drops" is the central claim, and a rate is no
    // evidence for it: discarding what cannot be linked would *raise* the
    // rate. The reported columns partition the extracted references, so their
    // sum is the reference count — exactly.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    typescript_tree(root);
    commonjs_tree(&root.join("cjs"));
    let db = root.join("graph.redb");
    let report = scan_ecma(root, &db).expect("scan");

    let mut stored = 0u64;
    let mut counted = 0u64;
    for (code, tally) in &report.per_lang {
        stored += tally.resolved + tally.external + tally.local_binding + tally.unresolved_total();
        counted += tally.resolved + tally.unresolved_total();
        assert!(Lang::from_code(*code).is_some());
    }

    let store = Store::open(&db).expect("store");
    let snapshot = store.snapshot().expect("snapshot");
    let rows: u64 = snapshot.rows.values().map(|r| u64::from(r.count)).sum();
    assert_eq!(stored, rows, "every row is counted exactly once");
    assert!(counted > 0, "something was measured");
}

#[test]
fn an_mjs_file_keeps_its_module_kind_into_the_second_phase() {
    // A5/A6: `.mjs` is normatively ESM whatever the nearest `package.json`
    // says, and ESM does no extension probing. The resolver's second phase
    // receives only the scope, so the kind has to be carried on it —
    // rebuilding a header from the path alone re-derived `Undecided`, sent
    // the file back to a `package.json` with no `"type"`, and resolved its
    // specifiers as CommonJS.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"nokind"}"#);
    write(root, "util.js", "export function parse() {}\n");
    write(root, "a.mjs", "import './util';\n");
    write(root, "b.cjs", "require('./util');\n");
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "a.mjs", "./util", "a.mjs"),
        "unresolved ModuleNotFound",
        "ESM resolves the specifier exactly or fails",
    );
    assert_eq!(
        row(&table, "b.cjs", "./util", "b.cjs"),
        "resolved util.js",
        "CommonJS probes extensions, and this file is normatively CommonJS",
    );
}

#[test]
fn a_static_and_an_instance_member_of_one_name_reach_different_nodes() {
    // The FQN carries the prototype, so `C.m()` and `new C().m()` are two
    // identities. Without it both members hashed to one node, one silently
    // won, and `this.m()` inside the instance method could reach the static
    // one.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"members"}"#);
    write(
        root,
        "m.ts",
        concat!(
            "export class C {\n",
            "  m(): void {}\n",
            "  static m(): void {}\n",
            "  run(): void { this.m() }\n",
            "  static go(): void { this.m() }\n",
            "}\n",
            "C.m();\n",
        ),
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "m.ts", "this.m", "m.ts#value:C.prototype.run"),
        "resolved m.ts#value:C.prototype.m",
        "`this` in an instance method is the prototype",
    );
    assert_eq!(
        row(&table, "m.ts", "this.m", "m.ts#value:C.go"),
        "resolved m.ts#value:C.m",
        "`this` in a static method is the constructor",
    );
    assert_eq!(
        row(&table, "m.ts", "C.m", "m.ts"),
        "resolved m.ts#value:C.m",
        "a qualified call names the static member",
    );
}

#[test]
fn this_walks_the_supertypes_this_build_indexed() {
    // A member inherited from a class declared right beside it is not
    // `UnindexedSupertype`: the supertype *is* indexed, and reporting it
    // unindexed both loses a real edge and files it under a reason that is
    // false. `UnindexedSupertype` still has to be reachable, or it would be
    // a reason that never fires.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"supers"}"#);
    write(
        root,
        "m.ts",
        concat!(
            "class B { m(): void {} }\n",
            "export class C extends B { f(): void { this.m() } }\n",
            "export class D extends B { g(): void { this.absent() } }\n",
            "export class E extends Unknowable { h(): void { this.m() } }\n",
            "export class F { k(): void { this.absent() } }\n",
        ),
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "m.ts", "this.m", "m.ts#value:C.prototype.f"),
        "resolved m.ts#value:B.prototype.m",
    );
    assert_eq!(
        row(&table, "m.ts", "this.absent", "m.ts#value:D.prototype.g"),
        "unresolved NoMatchingDefinition",
        "the whole chain is indexed and the member is genuinely absent",
    );
    assert_eq!(
        row(&table, "m.ts", "this.m", "m.ts#value:E.prototype.h"),
        "unresolved UnindexedSupertype",
        "a supertype this build never saw is where the member would be",
    );
    assert_eq!(
        row(&table, "m.ts", "this.absent", "m.ts#value:F.prototype.k"),
        "unresolved NoMatchingDefinition",
        "no heritage at all is a complete lookup",
    );
}

#[test]
fn a_type_only_import_cannot_be_constructed_and_can_be_queried() {
    // C17/C20. `import type` is elided at emit, so a site that survives
    // erasure cannot name it — and a site that is erased too still can:
    // `typeof X` is exactly what a type-only import is for.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"typeonly"}"#);
    write(root, "m.ts", "export class C {}\n");
    write(
        root,
        "use.ts",
        concat!(
            "import type { C } from './m.js';\n",
            "export type Ctor = typeof C;\n",
            "export function make(): void { new C() }\n",
        ),
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "use.ts", "C", "use.ts#value:make"),
        "unresolved NoMatchingDefinition",
        "an erased binding has nothing to construct at runtime",
    );
    assert!(
        outcomes(&table, "use.ts", "C", "use.ts#type:Ctor").contains(&"resolved m.ts#value:C"),
        "`typeof X` reads the Value space from a type position",
    );
}

#[test]
fn an_import_type_node_resolves_its_member() {
    // A23: the module is a literal and the member is written down, so
    // `NeedsExpressionType` would claim a type had to be inferred when
    // nothing did.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"importtype"}"#);
    write(root, "m.ts", "export interface Foo { a: number }\n");
    write(root, "use.ts", "export type T = import('./m.js').Foo;\n");
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "use.ts", "import('./m.js').Foo", "use.ts#type:T"),
        "resolved m.ts#type:Foo",
    );
}

#[test]
fn a_lowercase_jsx_element_is_external_and_a_component_is_not() {
    // C26: `<div/>` is a host intrinsic — nothing in this repository is
    // missing, which is exactly Node's builtin case one level over.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"jsx"}"#);
    write(
        root,
        "app.js",
        concat!(
            "function Button(){}\n",
            "export function App(){ return <div><Button /><Missing /></div> }\n",
        ),
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "app.js", "div", "app.js#value:App"),
        "external jsx:intrinsic",
    );
    assert_eq!(
        row(&table, "app.js", "Button", "app.js#value:App"),
        "resolved app.js#value:Button",
    );
    assert_eq!(
        row(&table, "app.js", "Missing", "app.js#value:App"),
        "unresolved NoMatchingDefinition",
        "a capitalised element is a binding, and a missing one is a miss",
    );
}

#[test]
fn a_module_level_var_in_a_block_is_one_definition_two_calls_reach() {
    // D3, end to end. Treating the block as the `var`'s scope emitted no
    // definition, so the call inside became `LocalBinding` — outside both
    // terms of the rate — and the call after it became
    // `NoMatchingDefinition`. One binding, two wrong answers, and the first
    // of them raised the rate.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"vars"}"#);
    write(
        root,
        "index.js",
        "{\n  var f = () => {};\n  f();\n}\nf();\n",
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        outcomes(&table, "index.js", "f", "index.js"),
        ["resolved index.js#value:f"],
        "both calls name the one module-level definition",
    );
}

#[test]
fn a_shadowed_require_stays_in_the_denominator() {
    // C1: `require` is the module wrapper's *parameter*. A module-level
    // declaration of that name shadows it, and reporting the call
    // `External("node:fs")` moved a real reference out of both terms of the
    // rate — the one way this gate can be cheated.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"shadow","type":"module"}"#);
    write(root, "a.js", "const require = () => {};\nrequire('fs');\n");
    write(root, "b.js", "require('fs');\n");
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "a.js", "require", "a.js"),
        "resolved a.js#value:require",
        "the call names the binding this file declared",
    );
    assert_eq!(row(&table, "b.js", "fs", "b.js"), "external node:fs");
}
