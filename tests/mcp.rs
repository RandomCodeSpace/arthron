//! `arthron mcp` as a client actually meets it: a real process, a scripted
//! session on its stdin, and the lines it wrote back.
//!
//! The dispatch rules are unit-tested in `src/mcp.rs`. What only a process can
//! show is here: that the handshake completes, that each of the four tools
//! returns the stage-2 document over the wire, that one response is one line,
//! and that a malformed frame or an unknown method is answered rather than
//! ending the session.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// `api#Handle → server#Serve → util#Parse`, the same tree the `--json` tests
/// use, so the documents can be compared against a known graph.
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

/// Run one session: every message in, every response out.
///
/// Each response is parsed on its own, which is the framing assertion — a
/// server that pretty-printed a response, or wrote two documents for one
/// request, would fail here rather than in some client's parser.
fn session(cwd: &Path, messages: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_arthron"))
        .arg("mcp")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the arthron binary");
    {
        let stdin = child.stdin.as_mut().expect("a piped stdin");
        for message in messages {
            writeln!(stdin, "{message}").expect("writing a message");
        }
    }
    // Dropping stdin is the end of input, which is the only thing that stops
    // the loop: a server that hung on a bad frame would hang here too.
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("waiting for the server");
    assert!(
        out.status.success(),
        "the server exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("a response is not one JSON object ({e}): {line}"))
        })
        .collect()
}

/// Raw lines in, so a session can send something that is not JSON at all.
fn raw_session(cwd: &Path, lines: &str) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_arthron"))
        .arg("mcp")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the arthron binary");
    child
        .stdin
        .as_mut()
        .expect("a piped stdin")
        .write_all(lines.as_bytes())
        .expect("writing the session");
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("waiting for the server");
    assert!(out.status.success(), "the server exited {:?}", out.status);
    String::from_utf8(out.stdout)
        .expect("stdout is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

fn handshake() -> Vec<Value> {
    vec![
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0" },
            },
        }),
        // A notification: it closes the handshake and is never answered.
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    ]
}

fn call(id: i64, tool: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    })
}

/// The document a successful call carries, checked against its text twin.
fn document(reply: &Value) -> Value {
    let result = &reply["result"];
    assert_eq!(result["isError"], false, "{result}");
    let text = result["content"][0]["text"].as_str().expect("text content");
    let parsed: Value = serde_json::from_str(text).expect("the text content is the document");
    assert_eq!(
        parsed, result["structuredContent"],
        "the two renderings of one document disagree",
    );
    parsed
}

#[test]
fn a_scripted_session_completes_the_handshake_and_answers_every_tool() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);

    let mut messages = handshake();
    messages.push(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    messages.push(call(
        3,
        "scan_repo",
        json!({ "path": root.to_str().unwrap() }),
    ));
    messages.push(call(4, "query_def", json!({ "name": "Parse" })));
    messages.push(call(5, "query_refs", json!({ "name": "Parse" })));
    messages.push(call(
        6,
        "query_impact",
        json!({ "name": "Parse", "depth": 2 }),
    ));
    let replies = session(root, &messages);

    // Six requests, six responses — the notification got none.
    assert_eq!(replies.len(), 6, "{replies:#?}");
    let ids: Vec<&Value> = replies.iter().map(|r| &r["id"]).collect();
    assert_eq!(
        ids,
        vec![
            &json!(1),
            &json!(2),
            &json!(3),
            &json!(4),
            &json!(5),
            &json!(6)
        ]
    );
    for reply in &replies {
        assert_eq!(reply["jsonrpc"], "2.0");
        assert!(reply.get("error").is_none(), "{reply}");
    }

    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "arthron");

    let names: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|t| t["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        ["scan_repo", "query_def", "query_refs", "query_impact"],
    );

    // scan_repo returns the scan document, unchanged from `scan --json`: the
    // fixture's four references, all four linked.
    let scanned = document(&replies[2]);
    assert_eq!(scanned["command"], "scan");
    assert_eq!(scanned["schema"], 1);
    assert_eq!(scanned["languages"]["go"]["resolved"], 4);
    assert_eq!(scanned["languages"]["go"]["unresolved"], 0);
    assert_eq!(scanned["languages"]["go"]["rate"], 1.0);

    // The query tools read the graph that scan just wrote, at the working
    // directory's default path.
    let def = document(&replies[3]);
    assert_eq!(def["command"], "query");
    assert_eq!(def["verb"], "def");
    assert_eq!(def["status"], "ok");
    assert_eq!(def["fqn"], "example.com/app/util#Parse");
    assert_eq!(def["declarations"][0]["file"], "util/util.go");

    let refs = document(&replies[4]);
    assert_eq!(refs["verb"], "refs");
    assert_eq!(refs["rows"], 1);
    assert_eq!(refs["references"][0]["file"], "server/server.go");
    assert_eq!(refs["references"][0]["outcome"]["status"], "resolved");

    // `Serve` calls `Parse`, and `Handle` calls `Serve`: two hops.
    let reached = document(&replies[5]);
    assert_eq!(reached["verb"], "impact");
    assert_eq!(reached["depth"], 2);
    assert_eq!(
        reached["layers"][0]["nodes"][0]["fqn"],
        "example.com/app/server#Serve"
    );
    assert_eq!(
        reached["layers"][1]["nodes"][0]["fqn"],
        "example.com/app/api#Handle"
    );
    assert_eq!(reached["truncated"], false);
}

#[test]
fn a_name_the_graph_does_not_hold_is_an_answer_and_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    // A second `Parse`, so one name genuinely selects two definitions.
    write(
        root,
        "text/text.go",
        "package text\n\nfunc Parse(s string) string { return s }\n",
    );

    let mut messages = handshake();
    messages.push(call(
        2,
        "scan_repo",
        json!({ "path": root.to_str().unwrap() }),
    ));
    messages.push(call(3, "query_def", json!({ "name": "NoSuchThing" })));
    messages.push(call(4, "query_def", json!({ "name": "Parse" })));
    let replies = session(root, &messages);
    assert_eq!(replies.len(), 4, "{replies:#?}");

    let absent = document(&replies[2]);
    assert_eq!(absent["status"], "no_match");
    assert_eq!(absent["matches"], json!([]));

    // Every candidate is named: the model has to pick, because the server
    // guessing would be the guess the resolver itself is forbidden to make.
    let several = document(&replies[3]);
    assert_eq!(several["status"], "ambiguous");
    let matches = several["matches"].as_array().expect("an array");
    assert_eq!(matches.len(), 2, "{matches:?}");
    assert_eq!(matches[0]["fqn"], "example.com/app/text#Parse");
    assert_eq!(matches[1]["fqn"], "example.com/app/util#Parse");
}

#[test]
fn a_query_with_no_graph_behind_it_is_a_tool_failure_carrying_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let mut messages = handshake();
    messages.push(call(2, "query_refs", json!({ "name": "Parse" })));
    let replies = session(dir.path(), &messages);

    assert_eq!(replies.len(), 2);
    let result = &replies[1]["result"];
    assert_eq!(result["isError"], true, "{result}");
    // Nothing was measured, so there is no document — an empty one would read
    // as an empty answer.
    assert!(result.get("structuredContent").is_none(), "{result}");
    assert!(
        !result["content"][0]["text"]
            .as_str()
            .expect("a message")
            .is_empty()
    );
}

#[test]
fn a_malformed_frame_is_answered_and_the_session_carries_on() {
    let dir = tempfile::tempdir().unwrap();
    let script = concat!(
        "{\"jsonrpc\": \"2.0\", \"id\": 1, \"method\": \"ping\"}\n",
        "{not json at all\n",
        "\n",
        "[1, 2]\n",
        "{\"jsonrpc\": \"2.0\", \"id\": 2, \"method\": \"ping\"}\n",
    );
    let replies = raw_session(dir.path(), script);

    // Four responses: two pings, one parse error, one invalid request. The
    // blank line was framing and is answered with nothing at all.
    assert_eq!(replies.len(), 4, "{replies:#?}");
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[0]["result"], json!({}));
    assert_eq!(replies[1]["error"]["code"], -32700);
    assert_eq!(replies[1]["id"], Value::Null);
    assert_eq!(replies[2]["error"]["code"], -32600);
    // The session survived both: the last request is answered normally.
    assert_eq!(replies[3]["id"], 2);
    assert_eq!(replies[3]["result"], json!({}));
}

#[test]
fn an_unknown_method_is_method_not_found_and_the_session_carries_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut messages = handshake();
    messages.push(json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" }));
    messages.push(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }));
    let replies = session(dir.path(), &messages);

    assert_eq!(replies.len(), 3, "{replies:#?}");
    assert_eq!(replies[1]["id"], 2);
    assert_eq!(replies[1]["error"]["code"], -32601);
    assert!(replies[1].get("result").is_none());
    assert_eq!(replies[2]["id"], 3);
    assert_eq!(replies[2]["result"]["tools"].as_array().unwrap().len(), 4);
}

#[test]
fn the_help_names_every_tool_and_its_arguments() {
    let out = Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args(["mcp", "--help"])
        .output()
        .expect("running the binary");
    let help = String::from_utf8_lossy(&out.stdout);
    for tool in ["scan_repo", "query_def", "query_refs", "query_impact"] {
        assert!(help.contains(tool), "mcp --help omits `{tool}`: {help}");
    }
    for argument in ["path", "name", "depth", "db"] {
        assert!(help.contains(argument), "mcp --help omits `{argument}`");
    }
    // The transport is part of the contract: stdio, and nothing else.
    assert!(help.contains("stdin"), "{help}");
    assert!(help.contains("socket"), "{help}");
}
