//! Python end to end: one small tree, scanned through the registry, asserted
//! reference by reference.
//!
//! The unit tests in `resolve.rs` hand a resolver a `HashSet` of FQN strings
//! and ask what it says. This asks a different question: does the *pipeline*
//! — walk, phase 0 layout, phase 1 nodes, phase 2 references, store — produce
//! those answers when nobody hands it a table. Every FQN below therefore had
//! to be built twice, once by `def_fqn` writing a node and once by the
//! resolver generating a candidate, and a disagreement between the two shows
//! up here as an unresolved reference rather than as a passing unit test.
//!
//! The second half is the part worth keeping honest: the case study's
//! canonical *unresolvable* shapes, each asserted to carry the reason that
//! names its own piece of work. A reason that drifts into `LocalBinding` or
//! `External` would raise the resolution rate without linking anything, and
//! these assertions are what makes that a failing build rather than a better
//! number.
//!
//! # Why the Python-only tree does not go through `scan_repo`
//!
//! It cannot, today. `scan_repo` runs every live track in registry order and
//! propagates the first phase-0 failure, and Go's phase 0 is
//! `read_to_string(root/go.mod)` — so with two live tracks a repository that
//! is *only* Python fails the whole scan with `reading go.mod: No such file or
//! directory`. `LayoutError`'s own documentation already anticipates this
//! ("in the long run this is a per-file reason rather than a scan abort");
//! what going live adds is that one language's missing manifest now aborts
//! another language's scan. It is a core defect, not Python's to fix from
//! inside its own module, so the Python-only tree is scanned through
//! [`scan_python`] — the track's registered entry point, the same function
//! `scan_repo` would call — and `going_live_leaves_go_alone` exercises the
//! registry path on a tree that has both manifests.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use arthron::model::{Domain, Lang, RefKind, node_id, reason_name};
use arthron::pipeline::scan_repo;
use arthron::store::{Store, StoredOutcome};
use arthron::track_python::resolve::scan_python;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A package `app` under the repository root, one loose script, and a
/// `pyproject.toml` that declares one dependency.
fn fixture(root: &Path) {
    write(
        root,
        "pyproject.toml",
        concat!(
            "[project]\n",
            "name = \"fixture\"\n",
            "dependencies = [\n",
            "    \"requests>=2.0\",\n",
            "]\n",
        ),
    );

    // A-01/A-12: the package `app` *is* this file, and it declares a function
    // `util` beside a submodule also called `util`. Python writes both
    // `app.util`; the grammar must not.
    write(
        root,
        "app/__init__.py",
        concat!(
            "from .core import Client as Client\n",
            "\n",
            "\n",
            "def util():\n",
            "    return 1\n",
        ),
    );
    write(root, "app/util.py", "def helper():\n    return 2\n");
    write(
        root,
        "app/core.py",
        concat!(
            "class Client:\n",
            "    def send(self, payload):\n",
            "        return payload\n",
        ),
    );
    write(
        root,
        "app/base.py",
        concat!(
            "class Base:\n",
            "    def render(self):\n",
            "        return \"\"\n"
        ),
    );
    // E-01 across files: `self.render()` is declared in `app.base`, and the
    // MRO that finds it is built from a base this file names.
    write(
        root,
        "app/view.py",
        concat!(
            "from .base import Base\n",
            "\n",
            "\n",
            "class View(Base):\n",
            "    def go(self):\n",
            "        return self.render()\n",
        ),
    );
    write(
        root,
        "app/service.py",
        concat!(
            "import app.util\n",
            "import os\n",
            "import requests\n",
            "from .core import Client\n",
            "from . import util\n",
            "\n",
            "\n",
            "def run(c: Client):\n",
            "    c.send(1)\n",
            "    app.util.helper()\n",
            "    os.path.join(\"a\", \"b\")\n",
            "    requests.get(\"http://x\")\n",
            "    util()\n",
            "    len([])\n",
        ),
    );
    write(
        root,
        "app/star.py",
        concat!(
            "from .core import *\n",
            "from os.path import *\n",
            "\n",
            "\n",
            "def use():\n",
            "    return Client()\n",
            "\n",
            "\n",
            "def elsewhere():\n",
            "    return join(\"a\", \"b\")\n",
        ),
    );
    write(
        root,
        "app/meta.py",
        concat!(
            "class Meta(type):\n",
            "    pass\n",
            "\n",
            "\n",
            "class Model(metaclass=Meta):\n",
            "    def save(self):\n",
            "        return self.injected()\n",
        ),
    );
    write(
        root,
        "app/hard.py",
        concat!(
            "import nowhere_at_all\n",
            "from .nothing import gone\n",
            "\n",
            "\n",
            "def untyped(value):\n",
            "    return value.render()\n",
            "\n",
            "\n",
            "def shadowed():\n",
            "    helper = make()\n",
            "    return helper()\n",
            "\n",
            "\n",
            "def expression():\n",
            "    return make().render()\n",
        ),
    );
    // A-07: no `__init__.py` here, so this file is in no package at all and is
    // named by its path.
    write(root, "tools/gen.py", "def generate():\n    return 3\n");
}

/// Every reference in the scanned tree as
/// `(file, kind, enclosing, raw target) → outcome`.
fn outcomes(db: &Path) -> BTreeMap<(String, u8, String, String), StoredOutcome> {
    let store = Store::open(db).expect("store opens");
    store
        .snapshot()
        .expect("snapshot")
        .rows
        .into_iter()
        .filter(|(_, record)| record.lang == Lang::Python.code())
        .map(|(key, record)| {
            (
                (key.file, key.kind, key.enclosing, key.raw_target),
                record.outcome,
            )
        })
        .collect()
}

/// The outcome of the one reference matching a file, kind and site text.
fn outcome(
    rows: &BTreeMap<(String, u8, String, String), StoredOutcome>,
    file: &str,
    kind: RefKind,
    raw: &str,
) -> StoredOutcome {
    let mut found: Vec<&StoredOutcome> = rows
        .iter()
        .filter(|((f, k, _, r), _)| f == file && *k == kind.code() && r == raw)
        .map(|(_, outcome)| outcome)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one `{raw}` in {file}, found {}",
        found.len()
    );
    found.pop().unwrap().clone()
}

fn resolved(fqn: &str) -> StoredOutcome {
    StoredOutcome::Resolved(node_id(Domain::Python, fqn))
}

/// Assert an unresolved outcome by reason name, so a failure prints the reason
/// rather than a bare code.
fn assert_reason(actual: &StoredOutcome, expected: &str, what: &str) {
    match actual {
        StoredOutcome::Unresolved(code) => {
            assert_eq!(reason_name(*code), expected, "{what}")
        }
        other => panic!("{what}: expected Unresolved({expected}), got {other:?}"),
    }
}

#[test]
fn a_python_tree_resolves_across_files_and_names_what_it_cannot() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let db = root.join("graph.redb");

    let report = scan_python(root, &db).expect("scan succeeds");
    let python = report
        .per_lang
        .get(&Lang::Python.code())
        .expect("Python reports a line of its own");
    assert!(python.resolved > 0, "nothing linked at all");

    // -- the grammar ------------------------------------------------------

    // Scoped: redb takes an exclusive lock per file, so this reader must be
    // dropped before `outcomes` opens the same store again.
    {
        let store = Store::open(&db).expect("store opens");
        let node = |fqn: &str| {
            store
                .node(&node_id(Domain::Python, fqn))
                .expect("store read")
                .is_some()
        };
        // F1: the submodule `app.util` and the function `util` of `app` are two
        // nodes. A dots-throughout grammar would give them one.
        assert!(node("app"), "the package is a node");
        assert!(node("app.util"), "the submodule is a node");
        assert!(node("app#util"), "the function is a node");
        assert_ne!(
            node_id(Domain::Python, "app.util"),
            node_id(Domain::Python, "app#util")
        );
        // A-01: there is no `app.__init__`.
        assert!(!node("app.__init__"), "an `__init__` module was invented");
        // C-15/D-05: the class chain joins with `.` inside one container.
        assert!(node("app.core#Client"));
        assert!(node("app.core#Client.send"));
        // A-07: a file under no package is named by its path, and still hosts
        // definitions.
        assert!(node("tools/gen.py"));
        assert!(node("tools/gen.py#generate"));
    }

    let rows = outcomes(&db);

    // -- imports ----------------------------------------------------------

    assert_eq!(
        outcome(&rows, "app/service.py", RefKind::Import, "app.util"),
        resolved("app.util"),
        "an absolute import of an in-repository module",
    );
    // B-23: the standard library is a frozen set, not "has no dot".
    assert_eq!(
        outcome(&rows, "app/service.py", RefKind::Import, "os"),
        StoredOutcome::External("py:std:os".to_string()),
    );
    // …and a declared dependency is external for a different reason.
    assert_eq!(
        outcome(&rows, "app/service.py", RefKind::Import, "requests"),
        StoredOutcome::External("requests".to_string()),
    );
    // B-05/B-06: the leading dot survives extraction and anchors here.
    assert_eq!(
        outcome(&rows, "app/service.py", RefKind::Import, ".core.Client"),
        resolved("app.core#Client"),
    );
    // B-03, verbatim §7.11: the attribute of the module before the submodule
    // of the same name. Both exist here, so the order is the whole test.
    assert_eq!(
        outcome(&rows, "app/service.py", RefKind::Import, ".util"),
        resolved("app#util"),
    );

    // -- calls ------------------------------------------------------------

    // E-05: `c` is a parameter, and reading its annotation is not inference.
    // This is the assertion that stops `LocalBinding` from swallowing it.
    assert_eq!(
        outcome(&rows, "app/service.py", RefKind::Call, "c.send"),
        resolved("app.core#Client.send"),
    );
    // E-07: a chain longer than two segments, resolvable because its prefix is
    // a module.
    assert_eq!(
        outcome(&rows, "app/service.py", RefKind::Call, "app.util.helper"),
        resolved("app.util#helper"),
    );
    assert_eq!(
        outcome(&rows, "app/service.py", RefKind::Call, "util"),
        resolved("app#util"),
    );
    assert_eq!(
        outcome(&rows, "app/service.py", RefKind::Call, "os.path.join"),
        StoredOutcome::External("py:std:os".to_string()),
    );
    // C-02: builtins are the outermost scope, so this is reached only after
    // every in-scope candidate has missed.
    assert_eq!(
        outcome(&rows, "app/service.py", RefKind::Call, "len"),
        StoredOutcome::External("py:builtins".to_string()),
    );
    // E-01 across files: the enclosing class is known, and its base lives in
    // another file. This is the call class Python's rate rests on.
    assert_eq!(
        outcome(&rows, "app/view.py", RefKind::Call, "self.render"),
        resolved("app.base#Base.render"),
    );
    // B-10: a star import of an in-repository module *is* enumerable — every
    // public name of `app.core` is a node — so this resolves rather than
    // hiding behind `WildcardImport`.
    assert_eq!(
        outcome(&rows, "app/star.py", RefKind::Call, "Client"),
        resolved("app.core#Client"),
    );

    // -- the honest floor -------------------------------------------------

    // E-06: the receiver is a parameter with no annotation. The largest bucket
    // and the correct one; it must not become `LocalBinding`, which would take
    // it out of both terms of the rate.
    assert_reason(
        &outcome(&rows, "app/hard.py", RefKind::Call, "value.render"),
        "NeedsTypeInference",
        "an unannotated receiver",
    );
    // …whereas the *whole* target being one block-bound name really is a local
    // binding, and is the only shape that is.
    assert_reason(
        &outcome(&rows, "app/hard.py", RefKind::Call, "helper"),
        "LocalBinding",
        "a name the block binds",
    );
    // I.2: a member on an expression result gets its own reason, so the
    // type-inference bucket stays the size it really is.
    assert_reason(
        &outcome(&rows, "app/hard.py", RefKind::Call, "make().render"),
        "NeedsExpressionType",
        "a member on a call result",
    );
    // B-23's other half: not standard library, not declared, never indexed.
    assert_reason(
        &outcome(&rows, "app/hard.py", RefKind::Import, "nowhere_at_all"),
        "UnknownPackage",
        "an undeclared third-party import",
    );
    // A relative import is in-repository by construction, so "unknown package"
    // would be a different and false claim.
    assert_reason(
        &outcome(&rows, "app/hard.py", RefKind::Import, ".nothing.gone"),
        "ModuleNotFound",
        "a relative import of a module that is not there",
    );
    // B-11: the export set of a star-imported standard-library module is not
    // in the graph, so a miss against it proves nothing.
    assert_reason(
        &outcome(&rows, "app/star.py", RefKind::Call, "join"),
        "WildcardImport",
        "a name a non-indexable star import may supply",
    );
    // F-11: §3.3.3 lets a metaclass add attributes with no source site.
    // Django's `ModelBase` is this shape, and `NoMatchingDefinition` would
    // blame the repository for a name that really is there.
    assert_reason(
        &outcome(&rows, "app/meta.py", RefKind::Call, "self.injected"),
        "Generated",
        "a member a metaclass may inject",
    );

    // -- nothing was dropped ----------------------------------------------

    // The contract, restated over a whole scan: every Python reference the
    // extractor emitted has exactly one stored outcome, and the two categories
    // outside the rate are both non-empty *and* both small enough that neither
    // is doing the work.
    assert!(
        python.local_binding > 0,
        "the local-binding line is measured"
    );
    assert!(python.external > 0, "the external line is measured");
    assert!(
        python.unresolved_total() > 0,
        "a fixture with no unresolved reference is not testing the floor",
    );
}

#[test]
fn going_live_leaves_go_alone() {
    // Two languages, one store, one scan. Python's rows must not disturb Go's,
    // and the report keeps one line per language because a combined number
    // would let a collapse in one hide behind the other.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "app/app.go",
        "package app\n\nfunc Run() { helper() }\n\nfunc helper() {}\n",
    );
    fixture(root);

    let mixed = scan_repo(root, &root.join("mixed.redb")).expect("mixed scan");

    let go_only = tempfile::tempdir().unwrap();
    write(
        go_only.path(),
        "go.mod",
        "module example.com/app\n\ngo 1.22\n",
    );
    write(
        go_only.path(),
        "app/app.go",
        "package app\n\nfunc Run() { helper() }\n\nfunc helper() {}\n",
    );
    let alone = scan_repo(go_only.path(), &go_only.path().join("go.redb")).expect("go scan");

    assert_eq!(
        mixed.per_lang.get(&Lang::Go.code()),
        alone.per_lang.get(&Lang::Go.code()),
        "Python going live changed Go's tally",
    );
    assert!(
        mixed.per_lang.contains_key(&Lang::Python.code()),
        "Python reports a line of its own",
    );
}
