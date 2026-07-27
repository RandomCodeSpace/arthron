//! End-to-end for the `query` subcommand.
//!
//! This drives the built binary rather than the library, because the exit
//! code and the printed columns *are* the product surface: an answer a person
//! cannot read, or a script cannot branch on, is not an answer.
//!
//! Library-level coverage of the same three verbs lives in `tests/query.rs`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// The same three-package module `tests/query.rs` uses:
/// `api#Handle → server#Serve → util#Parse`, with `server#helper` a second
/// caller of `util#Parse`.
fn fixture(root: &Path) {
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    write(
        root,
        "util/util.go",
        "package util\n\nfunc Parse(s string) string { return s }\n",
    );
    write(
        root,
        "server/server.go",
        concat!(
            "package server\n\n",
            "import \"example.com/app/util\"\n\n",
            "func Serve() {\n",
            "\tutil.Parse(\"x\")\n",
            "\thelper()\n",
            "}\n\n",
            "func helper() {\n",
            "\tutil.Parse(\"y\")\n",
            "}\n",
        ),
    );
    write(
        root,
        "api/api.go",
        concat!(
            "package api\n\n",
            "import \"example.com/app/server\"\n\n",
            "func Handle() {\n",
            "\tserver.Serve()\n",
            "}\n",
        ),
    );
}

fn arthron(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args(args)
        .output()
        .expect("running the arthron binary")
}

/// Scan a fixture tree and hand back the store path a query should read.
fn scanned(root: &Path) -> std::path::PathBuf {
    fixture(root);
    let db = root.join("graph.redb");
    let out = arthron(&["scan", root.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    db
}

fn query(db: &Path, args: &[&str]) -> (String, String, i32) {
    let mut full = vec!["query"];
    full.extend_from_slice(args);
    full.push("--db");
    full.push(db.to_str().unwrap());
    let out = arthron(&full);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().expect("the process exited normally"),
    )
}

#[test]
fn def_prints_the_record_and_its_declaration_site() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["def", "example.com/app/util#Parse"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("definition   example.com/app/util#Parse"),
        "{stdout}"
    );
    assert!(stdout.contains("kind         function"), "{stdout}");
    assert!(stdout.contains("declared     util/util.go:3"), "{stdout}");
}

#[test]
fn refs_prints_every_resolved_site() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["refs", "Parse"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("2 row(s), 2 occurrence(s)"), "{stdout}");
    assert!(stdout.contains("server/server.go:6"), "{stdout}");
    assert!(stdout.contains("server/server.go:11"), "{stdout}");
    assert!(stdout.contains("example.com/app/server#helper"), "{stdout}");
    assert!(stdout.contains("resolved"), "{stdout}");
}

#[test]
fn refs_on_a_node_nothing_calls_says_so_and_still_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["refs", "Handle"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("no stored row resolves here"), "{stdout}");
}

#[test]
fn impact_prints_one_block_per_hop() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["impact", "Parse"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("depth 1      2 node(s)"), "{stdout}");
    assert!(stdout.contains("depth 2      1 node(s)"), "{stdout}");
    assert!(stdout.contains("example.com/app/api#Handle"), "{stdout}");
    assert!(
        !stdout.contains("truncated"),
        "the closure is exhausted: {stdout}"
    );
}

#[test]
fn impact_declares_the_bound_it_stopped_at() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["impact", "Parse", "--depth", "1"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("depth 1      2 node(s)"), "{stdout}");
    assert!(!stdout.contains("depth 2"), "{stdout}");
    assert!(stdout.contains("truncated"), "{stdout}");
}

#[test]
fn an_ambiguous_name_lists_the_candidates_and_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    write(
        dir.path(),
        "text/text.go",
        "package text\n\nfunc Parse(s string) string { return s }\n",
    );
    let db = dir.path().join("graph.redb");
    let out = arthron(&[
        "scan",
        dir.path().to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
    ]);
    assert!(out.status.success());

    let (stdout, _, code) = query(&db, &["def", "Parse"]);
    assert_eq!(
        code, 1,
        "ambiguity is an answer, and not a zero one: {stdout}"
    );
    assert!(
        stdout.contains("ambiguous: 2 matches for \"Parse\""),
        "{stdout}"
    );
    // The candidates are on stdout so they can be piped into the re-run.
    assert!(stdout.contains("example.com/app/text#Parse"), "{stdout}");
    assert!(stdout.contains("example.com/app/util#Parse"), "{stdout}");
}

#[test]
fn a_name_that_is_not_there_exits_one_and_says_only_that() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, _, code) = query(&db, &["def", "NoSuchThing"]);
    assert_eq!(code, 1);
    assert!(stdout.contains("no match for \"NoSuchThing\""), "{stdout}");
}

#[test]
fn an_absent_store_is_a_usage_error_and_not_a_new_empty_graph() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("never-scanned.redb");

    let (_, stderr, code) = query(&db, &["def", "Parse"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("never-scanned.redb"), "{stderr}");
    // The whole point of the read-only open: a query must not conjure the
    // store it failed to find.
    assert!(
        !db.exists(),
        "a query created a store it was only asked to read"
    );
}

/// An exact match that wins over suffix candidates says so, in both modes.
///
/// Python records a third-party package under its bare name, so `sqlparse` is
/// a node and so is every `import sqlparse` alias that ends in it. The
/// external node is the answer — nothing else spells it — but a person who
/// meant the aliases has to be told they exist.
#[test]
fn an_exact_match_names_the_candidates_it_won_over() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(
        root,
        "pyproject.toml",
        "[project]\nname = \"fixture\"\ndependencies = [\n    \"sqlparse\",\n]\n",
    );
    write(root, "app/__init__.py", "");
    write(
        root,
        "app/mod.py",
        "import sqlparse\n\n\ndef use(s):\n    return sqlparse.parse(s)\n",
    );
    let db = root.join("graph.redb");
    let out = arthron(&["scan", root.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let (stdout, stderr, code) = query(&db, &["def", "sqlparse"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("definition   sqlparse"), "{stdout}");
    // The note, and the node it would otherwise have dropped without a word.
    assert!(stdout.contains("also"), "{stdout}");
    assert!(stdout.contains("app.mod#sqlparse"), "{stdout}");

    let (stdout, stderr, code) = query(&db, &["--json", "def", "sqlparse"]);
    assert_eq!(code, 0, "{stderr}");
    let doc: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(doc["status"], "ok");
    assert_eq!(
        doc["shadowed"],
        serde_json::json!([{ "fqn": "app.mod#sqlparse", "kind": "alias" }]),
        "{doc}",
    );
}

/// The ordinary answer carries an empty `shadowed`, not a missing one.
#[test]
fn an_answer_that_hid_nothing_still_carries_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());

    let (stdout, stderr, code) = query(&db, &["--json", "def", "Parse"]);
    assert_eq!(code, 0, "{stderr}");
    let doc: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(doc["shadowed"], serde_json::json!([]), "{doc}");
    // And the text answer stays exactly as short as it was.
    let (stdout, _, _) = query(&db, &["def", "Parse"]);
    assert!(!stdout.contains("also"), "{stdout}");
}

/// A `--db` on the command line is read without consulting the config file.
///
/// `query` and `mcp` take only `db` from `arthron.toml`, so a flag that names
/// the store leaves the file nothing to say — and a syntax error in a config
/// the run has no business reading must not be what stops an agent's server
/// from starting.
#[test]
fn an_explicit_db_does_not_read_the_working_directory_config() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned(dir.path());
    let elsewhere = tempfile::tempdir().unwrap();
    fs::write(
        elsewhere.path().join("arthron.toml"),
        "this is not = = toml\n",
    )
    .unwrap();

    for args in [
        vec!["query", "--db", db.to_str().unwrap(), "def", "Parse"],
        vec![
            "query",
            "--db",
            db.to_str().unwrap(),
            "--json",
            "def",
            "Parse",
        ],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_arthron"))
            .args(&args)
            .current_dir(elsewhere.path())
            .output()
            .expect("running the arthron binary");
        assert_eq!(
            out.status.code(),
            Some(0),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // The MCP server is the sharper case: it refused to start at all.
    let mut child = Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args(["mcp", "--db", db.to_str().unwrap()])
        .current_dir(elsewhere.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning the server");
    {
        use std::io::Write as _;
        let stdin = child.stdin.as_mut().expect("a piped stdin");
        writeln!(
            stdin,
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}}"
        )
        .unwrap();
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("waiting for the server");
    assert!(
        out.status.success(),
        "the server refused to start: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("\"id\":1"),
        "the server answered nothing",
    );
}

/// A reader that leaves early ends the answer; it does not crash the program.
///
/// `| head`, `| less` quit, `| grep -q` — all of them close the pipe while
/// this is still writing, and the `println!` family answers that by panicking.
/// Exit 101 and a backtrace is indistinguishable from a real crash to the
/// script reading the exit code.
///
/// The fixture is deliberately larger than a pipe buffer (64 KiB on Linux):
/// the writer is then provably still writing when the reader goes away, which
/// is what makes this a test rather than a race.
#[test]
fn a_reader_that_leaves_early_is_not_a_crash() {
    use std::io::BufRead as _;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut bulk = String::from("package bulk\n\nimport \"example.com/app/util\"\n\n");
    for i in 0..2000 {
        bulk.push_str(&format!(
            "func Caller{i}() {{\n\tutil.Parse(\"{i}\")\n}}\n\n"
        ));
    }
    write(root, "bulk/bulk.go", &bulk);
    let db = root.join("graph.redb");
    let out = arthron(&["scan", root.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    for extra in [Vec::new(), vec!["--json"]] {
        let mut args = vec!["query"];
        args.extend_from_slice(&extra);
        args.extend_from_slice(&["refs", "Parse", "--db", db.to_str().unwrap()]);
        let mut child = Command::new(env!("CARGO_BIN_EXE_arthron"))
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawning the arthron binary");
        let stdout = child.stdout.take().expect("a piped stdout");
        let mut reader = std::io::BufReader::new(stdout);
        let mut first = String::new();
        reader
            .read_line(&mut first)
            .expect("the first line arrives");
        // The reader leaves, exactly as `head -1` does.
        drop(reader);
        let out = child.wait_with_output().expect("waiting for the query");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {stderr}");
    }
}
