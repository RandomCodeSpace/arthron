//! One policy, four tier-1 tracks: what counts as a `LocalBinding`.
//!
//! The rule these files pin is stated once, on
//! [`arthron::UnresolvedReason::LocalBinding`]:
//!
//! > A reference is `LocalBinding` **iff** its target root is a name and, at
//! > the reference's site, some enclosing binding environment that is *not a
//! > node* binds the leftmost segment in the declaration space that segment is
//! > looked up in. Depth is irrelevant: `x`, `x.y` and `x.y.z` are all
//! > `LocalBinding` when `x` is. `this`/`super`/expression roots never are,
//! > and a field is never local because a field is a node.
//!
//! Before this file existed the tree implemented that rule three different
//! ways — Go and ECMA broadly, Java and Python only when the *whole* target
//! was the bound name — so `f.m()` was outside both terms of the resolution
//! rate in Go and inside them in Java. One number could not be compared with
//! another. Every assertion below is therefore a cross-language one: the same
//! shape, written in four languages, asserted to land in the same bucket.
//!
//! These are name-asserting on purpose. A test that counted `LocalBinding`
//! rows would pass just as well if the resolver had put a *resolvable* call
//! in the bucket, which is the exact failure — a rate rising because a
//! reference left both of its terms — that the policy exists to prevent. So
//! every non-local companion in each tree asserts the definition it links to.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use arthron::model::{RefKind, reason_name};
use arthron::store::{NodeRecord, Store, StoredOutcome};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Every reference row of one scan, rendered as the *name* it points at.
///
/// An id proves an edge exists; what this file is about is which definition
/// it reaches, so a `Resolved` row renders as its FQN.
struct Scan {
    rows: Vec<(String, u8, String)>,
}

fn render(
    files: &[(&str, &str)],
    scan: fn(&Path, &Path) -> Result<arthron::store::Report, String>,
) -> Scan {
    let dir = tempfile::tempdir().expect("a scratch directory");
    for (path, source) in files {
        write(dir.path(), path, source);
    }
    let db = dir.path().join("graph.redb");
    scan(dir.path(), &db).expect("scan");
    let store = Store::open(&db).expect("the store opens");
    let snapshot = store.snapshot().expect("snapshot");
    let names: BTreeMap<_, _> = snapshot
        .nodes
        .iter()
        .filter_map(|(id, record)| match record {
            NodeRecord::Definition { fqn, .. } => Some((*id, fqn.clone())),
            NodeRecord::Package { import_path, .. } => Some((*id, import_path.clone())),
            NodeRecord::External { .. } => None,
        })
        .collect();
    let rows = snapshot
        .rows
        .iter()
        .map(|(key, record)| {
            let outcome = match &record.outcome {
                StoredOutcome::Resolved(id) => format!(
                    "resolved {}",
                    names.get(id).map_or("<unnamed node>", String::as_str)
                ),
                StoredOutcome::External(package) => format!("external {package}"),
                StoredOutcome::Unresolved(code) => reason_name(*code).to_string(),
            };
            (key.raw_target.clone(), key.kind, outcome)
        })
        .collect();
    Scan { rows }
}

impl Scan {
    /// The outcome of the one row with this site text and kind. Panics when
    /// there is not exactly one: reading the first of two would assert about
    /// whichever the row order happened to put first.
    #[track_caller]
    fn one(&self, raw_target: &str, kind: RefKind) -> &str {
        let code = kind.code();
        let hits: Vec<&str> = self
            .rows
            .iter()
            .filter(|(raw, k, _)| raw == raw_target && *k == code)
            .map(|(_, _, outcome)| outcome.as_str())
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one `{raw_target}` {kind:?} row, found {}\n{}",
            hits.len(),
            self.dump(),
        );
        hits[0]
    }

    fn dump(&self) -> String {
        let mut out = String::from("rows:\n");
        for (raw, kind, outcome) in &self.rows {
            out.push_str(&format!("  kind={kind} {raw:?} -> {outcome}\n"));
        }
        out
    }
}

// -- Go ---------------------------------------------------------------------

/// Go already read the flag broadly. These are the pins that keep it that way
/// while the other three tracks are brought onto the same rule.
#[test]
fn go_reports_a_member_of_a_local_as_a_local_binding() {
    let scan = render(
        &[
            ("go.mod", "module example.com/app\n\ngo 1.22\n"),
            (
                "app.go",
                concat!(
                    "package app\n\n",
                    "type Conn struct{ inner *Conn }\n\n",
                    "func (c *Conn) Close() {}\n\n",
                    "var pool Conn\n\n",
                    "func Helper() {}\n\n",
                    "func Serve(conn *Conn) {\n",
                    "\tconn.Close()\n",
                    "\tconn.inner.Close()\n",
                    "\tpool.Close()\n",
                    "\tHelper()\n",
                    "}\n",
                ),
            ),
        ],
        arthron::pipeline::scan_go,
    );

    // A parameter's member, at one segment of depth and at two.
    assert_eq!(scan.one("conn.Close", RefKind::Call), "LocalBinding");
    assert_eq!(scan.one("conn.inner.Close", RefKind::Call), "LocalBinding");
    // A package-level var is a node, so its member is not local — and this
    // assertion is what makes the two above mean something.
    assert_eq!(
        scan.one("pool.Close", RefKind::Call),
        "NeedsTypeInference",
        "a package-level var is a node; its member is not a local binding",
    );
    assert_eq!(
        scan.one("Helper", RefKind::Call),
        "resolved example.com/app#Helper",
    );
}

// -- Java -------------------------------------------------------------------

/// The tree every Java case below is measured in.
///
/// `Client` and `Other` both declare `send()`, so a wrong edge is a *different
/// named definition* rather than a miss — which is what lets these assertions
/// tell a wrong link from a lowered rate.
fn java_tree() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "app/Client.java",
            "package app;\npublic class Client { public void send() {} }\n",
        ),
        (
            "app/Other.java",
            "package app;\npublic class Other { public void send() {} }\n",
        ),
        (
            "app/Service.java",
            concat!(
                "package app;\n",
                "public class Service {\n",
                "    Client field = new Client();\n",
                "    void helper() {}\n",
                "    public void run(Client param) {\n",
                "        Client local = new Client();\n",
                "        local.send();\n",
                "        param.send();\n",
                "        field.send();\n",
                "        this.field.send();\n",
                "        helper();\n",
                "    }\n",
                "}\n",
            ),
        ),
    ]
}

#[test]
fn java_reports_a_member_of_a_local_as_a_local_binding() {
    let scan = render(&java_tree(), arthron::track_java::scan_java);

    // The change this wave makes: a member of a local, and of a parameter.
    // Both were `resolved app.Client#send` — a real edge, and one Go, JS and
    // TypeScript had already taken out of both rate terms.
    assert_eq!(scan.one("local.send", RefKind::Call), "LocalBinding");
    assert_eq!(scan.one("param.send", RefKind::Call), "LocalBinding");
    // A field is a node (D-05). Its member stays in the rate and keeps its
    // edge — this is the assertion that stops the policy eating Java's
    // declared-type lookup wholesale.
    assert_eq!(
        scan.one("field.send", RefKind::Call),
        "resolved app#Client.send/0"
    );
    assert_eq!(
        scan.one("this.field.send", RefKind::Call),
        "resolved app#Client.send/0",
        "a `this` root names no local",
    );
    // §6.5.1: an identifier before `(` is a MethodName, so a local named
    // `helper` could not shadow it and this is not a local binding.
    assert_eq!(
        scan.one("helper", RefKind::Call),
        "resolved app#Service.helper/0"
    );
}

// -- Python -----------------------------------------------------------------

fn python_tree() -> Vec<(&'static str, &'static str)> {
    vec![
        ("pyproject.toml", "[project]\nname = \"fixture\"\n"),
        ("app/__init__.py", ""),
        (
            "app/core.py",
            "class Client:\n    def send(self, payload):\n        return payload\n",
        ),
        (
            "app/service.py",
            concat!(
                "from .core import Client\n",
                "\n",
                "shared = Client()\n",
                "\n",
                "\n",
                "def annotated(c: Client):\n",
                "    return c.send(1)\n",
                "\n",
                "\n",
                "def inferred():\n",
                "    made = Client()\n",
                "    return made.send(1)\n",
                "\n",
                "\n",
                "def module_level():\n",
                "    return shared.send(1)\n",
            ),
        ),
    ]
}

#[test]
fn python_reports_a_member_of_a_local_as_a_local_binding() {
    let scan = render(&python_tree(), arthron::track_python::resolve::scan_python);

    // E-05's annotation table used to answer this one with a real edge. Under
    // the uniform rule an annotated parameter is still a parameter, so the
    // member of it leaves both rate terms — the single largest movement this
    // wave makes to any published number.
    assert_eq!(scan.one("c.send", RefKind::Call), "LocalBinding");
    assert_eq!(scan.one("made.send", RefKind::Call), "LocalBinding");
    // A module-level binding *is* a node, so it is not a local, and its member
    // keeps whatever honest reason it had.
    assert_eq!(
        scan.one("shared.send", RefKind::Call),
        "NeedsTypeInference",
        "a module-level name is a node; its member is not a local binding",
    );
}

// -- TypeScript -------------------------------------------------------------

#[test]
fn typescript_reports_a_member_of_a_local_as_a_local_binding() {
    let scan = render(
        &[
            ("package.json", r#"{"name":"app","type":"module"}"#),
            (
                "core.ts",
                "export class Client { send() {} }\nexport function helper() {}\n",
            ),
            (
                "service.ts",
                concat!(
                    "import { Client, helper } from './core.js';\n",
                    "export const shared = new Client();\n",
                    "export function run(param: Client) {\n",
                    "  const local = new Client();\n",
                    "  local.send();\n",
                    "  param.send();\n",
                    "  shared.send();\n",
                    "  helper();\n",
                    "}\n",
                ),
            ),
        ],
        arthron::track_ecma::scan_ecma,
    );

    assert_eq!(scan.one("local.send", RefKind::Call), "LocalBinding");
    assert_eq!(scan.one("param.send", RefKind::Call), "LocalBinding");
    // A module top level is a node, and `shared` is one — so this reference
    // stays in the rate.
    assert_ne!(
        scan.one("shared.send", RefKind::Call),
        "LocalBinding",
        "a module-level binding is a node, not a local",
    );
    assert_eq!(
        scan.one("helper", RefKind::Call),
        "resolved core.ts#value:helper"
    );
}
