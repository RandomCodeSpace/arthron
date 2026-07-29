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
    // no file declares; it exists because `index.ts` exports it. The alias is
    // still a node — it has to be, or the barrel's own outgoing edge would
    // start nowhere — but the *edge from `main`* runs past it to the
    // definition, which is what makes a call graph through a barrel usable.
    assert_eq!(
        row(&table, main, "parseInput", run),
        "resolved packages/core/src/parse.ts#value:parse",
    );
    // --- B3/F2: the local name of a default import is unrelated to the
    // definition's name, so the binding table is the only way back. Two
    // aliases stand between the two: `index.ts` re-exports `default as
    // format`, and `format.ts`'s `default` is itself an alias for the
    // declaration's own name. Following both is what makes B3's point
    // reachable — the declaration really is called `reallyFormat`.
    assert_eq!(
        row(&table, main, "format", run),
        "resolved packages/core/src/format.ts#value:reallyFormat",
    );
    // --- B1: a direct named import needs no alias.
    assert!(
        outcomes(&table, main, "parse", run)
            .contains(&"resolved packages/core/src/parse.ts#value:parse"),
    );
    // --- F4: a namespace import, then a name through its export map. The
    // name is reached through the namespace rather than through a binding,
    // and it lands on the same definition either way — a member of a
    // namespace and a direct import of it are one identity, not two.
    assert_eq!(
        row(&table, main, "core.parseInput", run),
        "resolved packages/core/src/parse.ts#value:parse",
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
        // `default` is itself an alias for the declaration it was written on,
        // so the barrel's own edge runs through it to the function.
        "resolved packages/core/src/format.ts#value:reallyFormat",
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
    // B5: `index.ts` carries a bare `export *`, and its target — `shapes.ts`
    // — is in the repository, so the star *is* enumerable now: the walk
    // entered it, listed what it exports, and `nothingHere` is not there.
    // That makes `NoMatchingDefinition` the true statement and
    // `WildcardImport` the false one, which is the reverse of what it was
    // before the store carried alias targets. `WildcardImport` is not gone —
    // it is reserved for a star this build genuinely cannot enumerate, such
    // as a CommonJS spread or a target outside the repository.
    assert_eq!(
        row(&table, main, "core.nothingHere", run),
        "unresolved NoMatchingDefinition",
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
    // C3/F8: `module.exports = Parser`, constructed with `new`. The module's
    // `default` is an alias for the local `Parser` it was assigned from, and
    // following it puts the edge on the class rather than on the export slot.
    assert_eq!(
        row(&table, "index.js", "Parser", main),
        "resolved lib/parser.js#value:Parser",
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

#[test]
fn a_test_runners_injected_globals_name_the_package_that_injects_them() {
    // The universe scope was under-modelled: a name a *package* puts in the
    // global scope reached step 3, found nothing, and reported
    // `NoMatchingDefinition` — a reason whose contract says the lookup table
    // was complete and the name absent, which for mocha's `describe` is
    // false on both halves. `UnknownPackage` files it against the package the
    // definition is actually in.
    //
    // Still `Unresolved`, so the reference stays in both terms of the rate:
    // re-filing it as `External` would raise the gate without linking
    // anything. Note that in *this* fixture mocha is a declared dependency,
    // so `import { it } from 'mocha'` would answer `External("npm:mocha")`
    // rather than agreeing — see
    // `the_two_channels_that_turn_an_environment_on_do_not_agree_about_the_imported_form`,
    // which pins both channels and both spellings.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name":"suite","devDependencies":{"mocha":"^10"}}"#,
    );
    write(
        root,
        "test/app.test.js",
        "describe('app', function () {\n  before(function () {});\n  it('works', function () {});\n  after(function () {});\n});\n",
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    for name in ["describe", "it", "before", "after"] {
        let found = outcomes(&table, "test/app.test.js", name, "test/app.test.js");
        assert_eq!(
            found,
            ["unresolved UnknownPackage"],
            "{name} is mocha's, not this repository's missing definition",
        );
    }
}

#[test]
fn an_ambient_environment_is_off_until_its_package_is_declared() {
    // The model is an *environment*, not a list of common words. A repository
    // that declares no test runner has nothing injecting `describe`, so the
    // honest answer there is the one the reason's own contract describes:
    // the table was complete and the name is absent.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"plain"}"#);
    write(root, "app.js", "describe('x', function () {});\n");
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "app.js", "describe", "app.js"),
        "unresolved NoMatchingDefinition",
    );
}

#[test]
fn a_declaration_of_an_injected_name_wins_over_the_ambient_environment() {
    // The universe scope is consulted last, and the package-injected half of
    // it is consulted after the host half. A repository that declares its own
    // `it` gets its own `it`, whatever it happens to depend on.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name":"own","devDependencies":{"vitest":"^2"}}"#,
    );
    write(
        root,
        "app.js",
        "export function expect(x) { return x; }\nexport function use() { return expect(1); }\n",
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "app.js", "expect", "app.js#value:use"),
        "resolved app.js#value:expect",
    );
}

#[test]
fn a_typescript_project_may_turn_an_environment_on_without_a_dependency() {
    // A vendored workspace member carries its own `package.json` with no
    // dependencies at all, and states its ambient packages the way TypeScript
    // states them: `compilerOptions.types`. That is the same fact through the
    // other documented channel, so it turns the same environment on.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"member"}"#);
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"types":["vitest"]}}"#,
    );
    write(root, "app.test.ts", "test('x', () => { expect(1) })\n");
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    for name in ["test", "expect"] {
        assert_eq!(
            row(&table, "app.test.ts", name, "app.test.ts"),
            "unresolved UnknownPackage",
            "{name}",
        );
    }
}

#[test]
fn a_member_of_an_injected_global_names_the_same_package() {
    // The host half of the universe scope already lets the *head* decide:
    // `console.log` is `External("web:global")`, not a type question. The
    // package half sits in the same position and answers the same way — a
    // member reached through `vi` is in vitest, and calling it
    // `NeedsTypeInference` would put a package boundary in the type-gap
    // bucket.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name":"members","devDependencies":{"vitest":"^2"}}"#,
    );
    write(root, "app.test.js", "vi.fn();\nconsole.log(1);\n");
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "app.test.js", "vi.fn", "app.test.js"),
        "unresolved UnknownPackage",
    );
    assert_eq!(
        row(&table, "app.test.js", "console.log", "app.test.js"),
        "external web:global",
    );
}

#[test]
fn a_custom_condition_the_tsconfig_states_is_supplied_to_the_exports_walk() {
    // The largest single driver of zod's rate: a monorepo publishes its
    // sources under a private condition and points its own `tsconfig` at it,
    // so every intra-repository import resolves to a `.ts` file that is right
    // there. Hardcoding the condition list made the same import take the
    // `"types"` branch instead, which names a built artefact no scan of the
    // sources can see — one missing module, and every name reached through it
    // misses with it.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"root"}"#);
    write(
        root,
        "packages/lib/package.json",
        // zod's shape exactly: the source branch first, then the branches
        // that name built artefacts beside the manifest — files a scan of the
        // sources will not find, which is why the miss is `ModuleNotFound`
        // rather than a package boundary.
        r#"{"name":"lib","exports":{".":{"@lib/source":"./src/index.ts","types":"./index.d.cts","import":"./index.js"}}}"#,
    );
    write(
        root,
        "packages/lib/src/index.ts",
        "export function fromSource(): number { return 1 }\n",
    );
    write(
        root,
        "packages/app/tsconfig.json",
        r#"{"compilerOptions":{"customConditions":["@lib/source"]}}"#,
    );
    write(
        root,
        "packages/app/use.ts",
        "import { fromSource } from 'lib';\nexport function go(): number { return fromSource() }\n",
    );
    // The same import, from a directory whose tsconfig says nothing.
    write(
        root,
        "packages/plain/use.ts",
        "import { fromSource } from 'lib';\nexport function go(): number { return fromSource() }\n",
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "packages/app/use.ts", "lib", "packages/app/use.ts"),
        "resolved packages/lib/src/index.ts",
    );
    assert_eq!(
        row(
            &table,
            "packages/app/use.ts",
            "fromSource",
            "packages/app/use.ts#value:go"
        ),
        "resolved packages/lib/src/index.ts#value:fromSource",
    );
    // Without the option the `"types"` branch still wins, and it names a
    // built file this scan cannot see. The miss is `ModuleNotFound`, not a
    // silent fallback to the source.
    assert_eq!(
        row(
            &table,
            "packages/plain/use.ts",
            "lib",
            "packages/plain/use.ts"
        ),
        "unresolved ModuleNotFound",
    );
}

#[test]
fn a_custom_condition_reaches_the_imports_map_too() {
    // `#`-prefixed specifiers go through the same NODE algorithm and the same
    // condition set. One condition list, both maps.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r##"{"name":"app","imports":{"#dep":{"@app/source":"./src/dep.ts","default":"./dist/dep.js"}}}"##,
    );
    write(root, "src/dep.ts", "export const dep = 1;\n");
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"customConditions":["@app/source"]}}"#,
    );
    write(
        root,
        "src/use.ts",
        "import { dep } from '#dep';\nexport const used = dep;\n",
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "src/use.ts", "#dep", "src/use.ts"),
        "resolved src/dep.ts",
    );
}

#[test]
fn a_mixed_tree_measures_the_same_on_a_cold_store_and_a_warm_one() {
    // The two passes share one store and one identity space, and the wake set
    // each pass computes is filtered to the files that pass owns. So a
    // JavaScript row that probed an identity the TypeScript pass then
    // declared was invalidated and could not be re-resolved inside that scan:
    // its currency claim was left withdrawn, the *next* scan re-read it, and
    // the JavaScript rate went up with nothing in the tree having changed.
    //
    // A rate that depends on how many times it has been measured is not a
    // measurement. The scan converges before it returns.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"root"}"#);
    write(
        root,
        "packages/lib/package.json",
        r#"{"name":"lib","main":"./src/index.ts"}"#,
    );
    write(
        root,
        "packages/lib/src/index.ts",
        "export function fromTs(): number { return 1 }\n",
    );
    write(
        root,
        "app/a.js",
        "const { fromTs } = require('lib');\nfunction useIt() { return fromTs() }\nmodule.exports = { useIt };\n",
    );
    let db = root.join("graph.redb");
    let cold = scan_ecma(root, &db).expect("cold scan");
    let cold_rows = rows(&db);
    let warm = scan_ecma(root, &db).expect("warm scan");
    let warm_rows = rows(&db);

    assert_eq!(
        cold_rows, warm_rows,
        "the same tree resolved to two different graphs",
    );
    let js = Lang::JavaScript.code();
    assert_eq!(
        (
            cold.per_lang[&js].resolved,
            cold.per_lang[&js].unresolved_total()
        ),
        (
            warm.per_lang[&js].resolved,
            warm.per_lang[&js].unresolved_total()
        ),
        "the JavaScript rate moved between two scans of one tree",
    );
    assert_eq!(
        row(&cold_rows, "app/a.js", "lib", "app/a.js"),
        "resolved packages/lib/src/index.ts",
        "the cold scan is the one that has to be right",
    );
}

#[test]
fn the_two_channels_that_turn_an_environment_on_do_not_agree_about_the_imported_form() {
    // The exact scope of the claim above, pinned rather than asserted in
    // prose, because the two halves of `declares_ambient` behave differently
    // and only one of them is a clean precedent.
    //
    // Through **`compilerOptions.types`** — zod's channel, and the one the
    // reason argument rests on — the two spellings agree: the package is not
    // a declared dependency, so the *imported* form falls off the end of the
    // specifier rules with `UnknownPackage`, and the injected form joins it.
    //
    // Through **`package.json`** — vue-core's channel — they do not. A
    // declared dependency is the dependency boundary, and the specifier rules
    // answer `External("npm:<pkg>")` for the import and for every name reached
    // through it. The injected form still answers `Unresolved(UnknownPackage)`
    // and must: nothing was linked, so re-filing it as `External` would take
    // it out of both terms of the rate and raise the gate for free. The cost
    // is that one repository carries both classes for one definition, and
    // closing that would mean moving the *imported* side — a change to what
    // `External` means at the dependency boundary for every package, not a
    // property of ambient globals. This test is here so that asymmetry cannot
    // change silently in either direction.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"types-channel"}"#);
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"types":["vitest"]}}"#,
    );
    write(
        root,
        "imported.test.ts",
        "import { describe, expect } from 'vitest';\ndescribe('x', () => { expect(1) });\n",
    );
    write(
        root,
        "injected.test.ts",
        "describe('y', () => { expect(1) });\n",
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "imported.test.ts", "vitest", "imported.test.ts"),
        "unresolved UnknownPackage",
        "vitest is not a declared dependency here, only a stated ambient type",
    );
    for (file, name) in [
        ("imported.test.ts", "describe"),
        ("imported.test.ts", "expect"),
        ("injected.test.ts", "describe"),
        ("injected.test.ts", "expect"),
    ] {
        assert_eq!(
            row(&table, file, name, file),
            "unresolved UnknownPackage",
            "{file} {name}: through `types` the two spellings agree",
        );
    }

    // The other channel, same names, same package, same repository.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name":"dep-channel","devDependencies":{"vitest":"^2"}}"#,
    );
    write(
        root,
        "imported.test.ts",
        "import { describe, expect } from 'vitest';\ndescribe('x', () => { expect(1) });\n",
    );
    write(
        root,
        "injected.test.ts",
        "describe('y', () => { expect(1) });\n",
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "imported.test.ts", "vitest", "imported.test.ts"),
        "external npm:vitest",
        "a declared dependency is the dependency boundary",
    );
    for name in ["describe", "expect"] {
        assert_eq!(
            row(&table, "imported.test.ts", name, "imported.test.ts"),
            "external npm:vitest",
            "{name} written as an import rides the declared dependency out",
        );
        assert_eq!(
            row(&table, "injected.test.ts", name, "injected.test.ts"),
            "unresolved UnknownPackage",
            "{name} injected stays in both terms of the rate",
        );
    }
}

#[test]
fn a_child_tsconfig_that_states_no_ambient_types_is_not_given_the_bases() {
    // `"types": []` is the documented way to say "no ambient type packages",
    // and `extends` is nearest-wins. Reading an empty list as "unstated" —
    // the emptiness proxy `paths` can afford — handed the child back the
    // `["jest"]` it had just switched off, and turned an ambient environment
    // on under a config that turned it off. tsc's own
    // `parseJsonConfigFileContent` resolves this child to `types: []`.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"root"}"#);
    write(
        root,
        "tsconfig.base.json",
        r#"{"compilerOptions":{"types":["jest"]}}"#,
    );
    write(
        root,
        "pkg/tsconfig.json",
        r#"{"extends":"../tsconfig.base.json","compilerOptions":{"types":[]}}"#,
    );
    write(root, "pkg/a.ts", "describe('x', () => {});\n");
    // The sibling states nothing, so it does inherit — the control that keeps
    // this test from passing because inheritance broke altogether.
    write(
        root,
        "kept/tsconfig.json",
        r#"{"extends":"../tsconfig.base.json"}"#,
    );
    write(root, "kept/b.ts", "describe('y', () => {});\n");
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "pkg/a.ts", "describe", "pkg/a.ts"),
        "unresolved NoMatchingDefinition",
        "the child turned jest off, so nothing injects `describe` there",
    );
    assert_eq!(
        row(&table, "kept/b.ts", "describe", "kept/b.ts"),
        "unresolved UnknownPackage",
        "the sibling states nothing and still inherits the base's `types`",
    );
}

#[test]
fn a_child_tsconfig_that_states_no_custom_conditions_is_not_given_the_bases() {
    // The same rule on the option that moves the *rate*: a child stating
    // `"customConditions": []` resolves to `[]` under tsc, so its imports take
    // the branch the package author wrote first. Inheriting the base's list
    // instead sent them down the private source branch and linked a file tsc
    // would not have reached — a rate that goes up for a reason the project
    // did not state.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"root"}"#);
    write(
        root,
        "tsconfig.base.json",
        r#"{"compilerOptions":{"customConditions":["@lib/source"]}}"#,
    );
    write(
        root,
        "packages/lib/package.json",
        r#"{"name":"lib","exports":{".":{"@lib/source":"./src/index.ts","types":"./index.d.ts","default":"./index.js"}}}"#,
    );
    write(
        root,
        "packages/lib/src/index.ts",
        "export function v(): number { return 1 }\n",
    );
    write(
        root,
        "packages/lib/index.d.ts",
        "export declare function v(): number;\n",
    );
    write(
        root,
        "off/tsconfig.json",
        r#"{"extends":"../tsconfig.base.json","compilerOptions":{"customConditions":[]}}"#,
    );
    write(
        root,
        "off/use.ts",
        "import { v } from 'lib';\nexport const a = v();\n",
    );
    write(
        root,
        "on/tsconfig.json",
        r#"{"extends":"../tsconfig.base.json"}"#,
    );
    write(
        root,
        "on/use.ts",
        "import { v } from 'lib';\nexport const a = v();\n",
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(&table, "off/use.ts", "lib", "off/use.ts"),
        "resolved packages/lib/index.d.ts",
        "the child switched the condition off, so `\"types\"` wins as tsc has it",
    );
    assert_eq!(
        row(&table, "on/use.ts", "lib", "on/use.ts"),
        "resolved packages/lib/src/index.ts",
        "the sibling states nothing and still inherits the base's condition",
    );
}

#[test]
fn a_custom_condition_does_not_outrank_the_maps_own_key_order() {
    // The option contributes to a **set**. NODE walks the conditions object in
    // the *map's* key order and takes the first key the set contains, so a
    // package that writes `"types"` ahead of its private source condition
    // keeps the `"types"` branch even for a project that states the condition.
    //
    // Matching in the caller's order instead would silently retarget every
    // such import at the source file the author put second. That drifts the
    // graph toward more in-repo `.ts` targets — the rate goes *up*, which no
    // gate fails on — so the only thing that can catch it is a map that
    // disagrees with the caller about the order, which is what this is. Every
    // other condition fixture here, and zod's whole `exports` map, writes the
    // custom condition first and cannot see the difference.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"root"}"#);
    write(
        root,
        "packages/lib/package.json",
        r#"{"name":"lib","exports":{"./first":{"types":"./first.d.ts","@lib/source":"./src/first.ts"},"./second":{"@lib/source":"./src/second.ts","types":"./second.d.ts"}}}"#,
    );
    write(
        root,
        "packages/lib/first.d.ts",
        "export declare const first: number;\n",
    );
    write(
        root,
        "packages/lib/src/first.ts",
        "export const first = 1;\n",
    );
    write(
        root,
        "packages/lib/second.d.ts",
        "export declare const second: number;\n",
    );
    write(
        root,
        "packages/lib/src/second.ts",
        "export const second = 2;\n",
    );
    write(
        root,
        "packages/app/tsconfig.json",
        r#"{"compilerOptions":{"customConditions":["@lib/source"]}}"#,
    );
    write(
        root,
        "packages/app/use.ts",
        "import { first } from 'lib/first';\nimport { second } from 'lib/second';\nexport const a = first + second;\n",
    );
    let db = root.join("graph.redb");
    scan_ecma(root, &db).expect("scan");
    let table = rows(&db);

    assert_eq!(
        row(
            &table,
            "packages/app/use.ts",
            "lib/first",
            "packages/app/use.ts"
        ),
        "resolved packages/lib/first.d.ts",
        "`\"types\"` is written first, so it wins over the stated condition",
    );
    assert_eq!(
        row(
            &table,
            "packages/app/use.ts",
            "lib/second",
            "packages/app/use.ts"
        ),
        "resolved packages/lib/src/second.ts",
        "the same condition still wins where the author wrote it first",
    );
}

#[test]
fn a_file_error_from_any_pass_reaches_the_report() {
    // Three passes walk the tree — JavaScript, TypeScript, then JavaScript
    // again to converge — and each has file errors the others never see. The
    // report used to be the last pass's alone, and that pass re-reads only the
    // files whose claims are outstanding: on an ordinary tree it re-reads
    // nothing, so *every* error either of the first two passes found vanished
    // from the report while the file it names stayed unread.
    //
    // A file the scan could not read is the one thing a resolution rate cannot
    // account for on its own, so it has to survive to the report.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"root"}"#);
    write(root, "good.ts", "export const ok = 1;\n");
    fs::write(root.join("bad.js"), [0x2f, 0x2f, 0xff, 0xfe, 0x0a]).unwrap();
    fs::write(root.join("bad.ts"), [0x2f, 0x2f, 0xff, 0xfe, 0x0a]).unwrap();
    let db = root.join("graph.redb");
    let report = scan_ecma(root, &db).expect("scan");

    let paths: Vec<&str> = report.file_errors.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        ["bad.js", "bad.ts"],
        "each pass's unreadable file has to reach the report: {:?}",
        report.file_errors,
    );

    // And the other direction on its own: a tree whose only unreadable file
    // belongs to the *first* pass. Nothing wakes the converging pass here, so
    // it re-reads nothing and has nothing to report — the error reaches the
    // report only because the passes are unioned.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name":"root"}"#);
    write(root, "a.ts", "export const ok = 1;\n");
    write(
        root,
        "b.ts",
        "import { ok } from './a';\nexport const used = ok;\n",
    );
    fs::write(root.join("only.js"), [0x2f, 0x2f, 0xff, 0xfe, 0x0a]).unwrap();
    let db = root.join("graph.redb");
    let report = scan_ecma(root, &db).expect("scan");

    let paths: Vec<&str> = report.file_errors.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        ["only.js"],
        "the JavaScript pass's error survived to the report: {:?}",
        report.file_errors,
    );
}
