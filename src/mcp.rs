//! `arthron mcp`: the stored graph as a Model Context Protocol server, on
//! stdio.
//!
//! # What this layer is
//!
//! A transport, and nothing else. Every tool here builds the same document
//! [`crate::json`] builds for `--json`, from the same library calls the CLI
//! makes. There is no second answer for agents: if a number is wrong here it
//! is wrong on the command line too, which is the only way two surfaces stay
//! honest about one graph.
//!
//! # Framing
//!
//! JSON-RPC 2.0, one message per line, stdin in and stdout out. Lines only —
//! no `Content-Length` headers, which the MCP stdio transport does not use.
//! Responses are compact so a response is exactly one line; the document
//! inside a tool result is pretty-printed into a JSON *string*, where its
//! newlines are escaped and cannot break the framing.
//!
//! **stdio only.** No socket is opened and no address is bound, here or
//! anywhere else in this binary. The no-network rule is not a configuration.
//!
//! # Answering rather than dying
//!
//! Nothing a client can send may end the loop. A line that is not UTF-8, is
//! not JSON, or is not a JSON-RPC request is answered with a JSON-RPC error
//! and the server reads the next line. Only end-of-input stops it.
//!
//! One deliberate silence: a message with no `id` is a notification, and
//! JSON-RPC forbids answering a notification — including one this build does
//! not understand. So `notifications/initialized` and everything like it are
//! read and dropped. A *request* naming an unknown method is answered with
//! method-not-found.
//!
//! # What is an error, and what is an answer
//!
//! The same split `--json` makes. A query whose name matches nothing, or
//! matches several nodes, is an **answer**: the document says so, and
//! `isError` is false, because the model needs to read the candidate list to
//! ask again. A store that will not open, or a scan that fails, measured
//! nothing, so it is `isError: true` carrying the message and no document.
//! Malformed arguments never reach the graph at all and are a JSON-RPC
//! `-32602` instead.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::json;
use crate::pipeline::scan_repo_with;
use crate::query::{DEFAULT_IMPACT_DEPTH, NameIndex, definition, impact, references};
use crate::store::ReadStore;

/// The MCP revision this server names when a client asks for one it does not
/// know.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The revisions this server will speak if asked.
///
/// All three define `initialize`, `tools/list` and `tools/call` identically
/// for a server whose only capability is tools, which is the whole of this
/// surface — so echoing back an older one is a claim that holds. A revision
/// outside this list is answered with [`PROTOCOL_VERSION`] and the client
/// decides whether to continue, which is what the specification asks for.
pub const COMPATIBLE: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// What the server tells a client its tools are for.
const INSTRUCTIONS: &str = concat!(
    "arthron answers questions about a code graph it resolved locally. ",
    "Call scan_repo first: the query tools read a stored graph and cannot ",
    "build one. A reference the resolver could not link is reported with a ",
    "reason rather than dropped, so an unresolved count is data, not a gap. ",
    "A name that matches several definitions comes back with every candidate ",
    "and status \"ambiguous\" — pick one and ask again with its full FQN."
);

// JSON-RPC 2.0 error codes. Only the four this server can actually produce.
/// The line was not JSON.
const PARSE_ERROR: i64 = -32700;
/// The message was JSON, but not a JSON-RPC request.
const INVALID_REQUEST: i64 = -32600;
/// A request named a method this build does not have.
const METHOD_NOT_FOUND: i64 = -32601;
/// A request named a real method with arguments it cannot take.
const INVALID_PARAMS: i64 = -32602;

/// The `--help` text: what the server speaks and every tool it offers.
pub const HELP: &str = concat!(
    "Serve the stored graph over the Model Context Protocol on stdin/stdout.\n",
    "\n",
    "JSON-RPC 2.0, one message per line. No socket is opened and no address is\n",
    "bound: like every other command here, the server talks to nothing but the\n",
    "process that started it.\n",
    "\n",
    "Tools\n",
    "  scan_repo     Build or refresh a repository's graph and return the scan\n",
    "                document: per-language resolved / external / local_binding\n",
    "                / unresolved counts, every unresolved reason, and the rate.\n",
    "                path  (string, required)  repository root\n",
    "                db    (string, optional)  where to write the graph;\n",
    "                                          default <path>/.arthron/graph.redb\n",
    "  query_def     A name's definition record, every site that declares it,\n",
    "                and what it forwards to when it is an alias.\n",
    "                name  (string, required)\n",
    "  query_refs    Every stored reference row that resolved to the name, with\n",
    "                file, line, kind, enclosing definition and raw target.\n",
    "                name  (string, required)\n",
    "  query_impact  What transitively reaches the name, layer by layer, and\n",
    "                whether the depth bound cut the walk short.\n",
    "                name  (string, required)\n",
    "                depth (integer, optional) default 3\n",
    "\n",
    "A tool returns the same JSON document `--json` prints, as text content and\n",
    "again as structuredContent. A name matching nothing or matching several\n",
    "nodes is an answer, not an error: the document carries status \"no_match\"\n",
    "or \"ambiguous\" with every candidate. isError is set only when nothing was\n",
    "measured at all — a store that would not open, or a scan that failed.\n",
    "\n",
    "The three query tools read one graph for the whole session: --db if given,\n",
    "else the working directory's arthron.toml `db`, else .arthron/graph.redb.\n",
    "scan_repo writes wherever its own arguments say.",
);

/// A JSON-RPC failure: the protocol refused the request before any work ran.
struct RpcError {
    code: i64,
    message: String,
}

/// The arguments were not something a tool can take.
fn invalid_params(message: String) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message,
    }
}

/// Which question a query tool asks. One enum rather than a string so the
/// dispatch cannot grow a case that silently means "impact".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Def,
    Refs,
    Impact(u32),
}

impl Verb {
    /// The name the document records, identical to the CLI's verb.
    fn name(self) -> &'static str {
        match self {
            Verb::Def => "def",
            Verb::Refs => "refs",
            Verb::Impact(_) => "impact",
        }
    }
}

/// The longest frame this server will hold in memory, in bytes.
///
/// Reading a line with no bound makes the client's stdin an allocator: a frame
/// with no newline in it is buffered whole, so 600 MB of anything — a binary
/// file redirected into stdin, a client bug, a `name` argument built in a
/// loop — becomes 600 MB of resident memory and breaches this project's hard
/// ceiling of 512 MB RSS. There is no authentication on a stdio transport and
/// there does not need to be; the bound is what makes that safe.
///
/// One MiB, because every message this server accepts is a control message —
/// a path, a name, a depth — and the largest legitimate one is a few hundred
/// bytes. A frame over the bound is discarded as it arrives rather than
/// accumulated, so the peak stays flat no matter how long it is.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// How much buffer the frame reader keeps between messages.
///
/// A `Vec` that grew to hold a large frame keeps that capacity for the rest of
/// the session, so one big message would pin the memory it needed forever.
/// Anything past this is released once the frame is answered.
const IDLE_FRAME_CAPACITY: usize = 64 * 1024;

/// What one read off the transport produced.
enum Frame {
    /// A complete line, in the caller's buffer.
    Line,
    /// A line longer than [`MAX_FRAME_BYTES`]. Its bytes were consumed up to
    /// and including the newline, so the *next* read starts on a real frame
    /// boundary and the session stays in step.
    Oversized,
    /// End of input, the only thing that stops the loop.
    Eof,
}

/// Read one newline-terminated frame, never buffering more than
/// [`MAX_FRAME_BYTES`] of it.
///
/// `BufRead::read_until` cannot do this: it has no bound, and by the time it
/// returns the memory is already committed. So the reader walks the buffered
/// chunks itself, stops appending once the bound is passed, and keeps
/// consuming until the newline — which is what turns an oversized frame into
/// an answerable error rather than a desynchronised session.
fn read_frame(input: &mut dyn BufRead, buf: &mut Vec<u8>) -> Result<Frame, String> {
    buf.clear();
    if buf.capacity() > IDLE_FRAME_CAPACITY {
        buf.shrink_to(IDLE_FRAME_CAPACITY);
    }
    let mut oversized = false;
    loop {
        // The borrow on `input` has to end before `consume`, so the chunk is
        // inspected and copied inside this block and only the counts escape.
        let (newline, used) = {
            let available = input
                .fill_buf()
                .map_err(|e| format!("reading stdin: {e}"))?;
            if available.is_empty() {
                // End of input. A frame with no trailing newline is still a
                // frame — and still answerable — so what is in hand decides.
                return Ok(if oversized {
                    Frame::Oversized
                } else if buf.is_empty() {
                    Frame::Eof
                } else {
                    Frame::Line
                });
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(at) => {
                    if !oversized {
                        buf.extend_from_slice(&available[..=at]);
                    }
                    (true, at + 1)
                }
                None => {
                    if !oversized {
                        buf.extend_from_slice(available);
                    }
                    (false, available.len())
                }
            }
        };
        input.consume(used);
        if !oversized && buf.len() > MAX_FRAME_BYTES {
            // Past the bound: drop what was kept and stop keeping more. The
            // rest of the frame is still read, and thrown away as it arrives.
            oversized = true;
            buf.clear();
            buf.shrink_to(IDLE_FRAME_CAPACITY);
        }
        if newline {
            return Ok(if oversized {
                Frame::Oversized
            } else {
                Frame::Line
            });
        }
    }
}

/// The server: a graph to read, and a loop that answers questions about it.
pub struct Server {
    /// The store the three query tools read. Fixed for the session, exactly
    /// as a `query` invocation's is fixed for the run.
    db: PathBuf,
}

impl Server {
    /// A server that answers queries against `db`.
    ///
    /// The path is not opened here. A server whose store does not exist yet is
    /// a normal state — the client's first call is usually `scan_repo`, which
    /// creates it — so the store is opened per call and a missing one is a
    /// tool failure with a message, not a refusal to start.
    pub fn new(db: PathBuf) -> Self {
        Server { db }
    }

    /// Read messages until end of input, answering each one.
    ///
    /// Returns `Err` only for a broken pipe or an unreadable stdin: a failure
    /// of the transport itself, which no error message could travel over.
    /// Every failure a *client* can cause is answered in-band and the loop
    /// continues.
    pub fn run(&self, input: &mut dyn BufRead, output: &mut dyn Write) -> Result<(), String> {
        let mut line = Vec::new();
        loop {
            let response = match read_frame(input, &mut line)? {
                Frame::Eof => return Ok(()),
                Frame::Line => match std::str::from_utf8(&line) {
                    Ok(text) => self.handle(text),
                    Err(e) => Some(error(
                        Value::Null,
                        PARSE_ERROR,
                        format!("the line is not valid UTF-8: {e}"),
                    )),
                },
                Frame::Oversized => Some(error(
                    Value::Null,
                    PARSE_ERROR,
                    format!(
                        "the frame is longer than {MAX_FRAME_BYTES} bytes and was discarded \
                         unread; one JSON-RPC message per line"
                    ),
                )),
            };
            if let Some(doc) = response {
                // Compact, and one `writeln!`: a response is one line, and the
                // framing is the only thing keeping a client in step.
                let text = serde_json::to_string(&doc)
                    .map_err(|e| format!("serialising the response: {e}"))?;
                writeln!(output, "{text}").map_err(|e| format!("writing stdout: {e}"))?;
                output
                    .flush()
                    .map_err(|e| format!("flushing stdout: {e}"))?;
            }
        }
    }

    /// Answer one line, or `None` when it must not be answered.
    ///
    /// `None` means one of two things, and both are silence by the
    /// specification: the line was blank framing, or the message was a
    /// notification.
    pub fn handle(&self, line: &str) -> Option<Value> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                return Some(error(
                    Value::Null,
                    PARSE_ERROR,
                    format!("invalid JSON: {e}"),
                ));
            }
        };
        let Some(object) = message.as_object() else {
            return Some(error(
                Value::Null,
                INVALID_REQUEST,
                "a message must be a JSON object; batches are not supported".to_string(),
            ));
        };

        // An `id` is what makes a message answerable. Absent means a
        // notification; present but neither a string nor a number is not a
        // JSON-RPC id at all, and MCP forbids null besides — so there is no id
        // to answer with and the refusal carries null.
        let id = match object.get("id") {
            None => None,
            Some(value @ (Value::String(_) | Value::Number(_))) => Some(value.clone()),
            Some(_) => {
                return Some(error(
                    Value::Null,
                    INVALID_REQUEST,
                    "`id` must be a string or a number".to_string(),
                ));
            }
        };

        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return id.map(|id| {
                error(
                    id,
                    INVALID_REQUEST,
                    "`jsonrpc` must be the string \"2.0\"".to_string(),
                )
            });
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return id.map(|id| {
                error(
                    id,
                    INVALID_REQUEST,
                    "`method` is missing or not a string".to_string(),
                )
            });
        };
        // A notification is never answered — not even to say the method is
        // unknown. `notifications/initialized` closes the handshake and needs
        // nothing done; anything else is dropped by the same rule.
        let id = id?;

        let empty = Value::Object(Map::new());
        let params = object.get("params").unwrap_or(&empty);
        let outcome = match method {
            "initialize" => Ok(initialize(params)),
            // Part of the base protocol: a client's keep-alive must not come
            // back as an unknown method.
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.call(params),
            other => {
                return Some(error(
                    id,
                    METHOD_NOT_FOUND,
                    format!("unknown method `{other}`"),
                ));
            }
        };
        Some(match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(e) => error(id, e.code, e.message),
        })
    }

    /// Run one tool and wrap what it produced as a `tools/call` result.
    fn call(&self, params: &Value) -> Result<Value, RpcError> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err(invalid_params(
                "`name` is missing or not a string".to_string(),
            ));
        };
        let empty = Value::Object(Map::new());
        let args = params.get("arguments").unwrap_or(&empty);
        if !args.is_object() {
            return Err(invalid_params("`arguments` must be an object".to_string()));
        }
        let produced = match name {
            "scan_repo" => {
                known_arguments(args, &["path", "db"])?;
                let path = string_argument(args, "path")?;
                let db = optional_string_argument(args, "db")?;
                scan(Path::new(&path), db.as_deref().map(Path::new))
            }
            "query_def" => {
                known_arguments(args, &["name"])?;
                self.query(Verb::Def, &string_argument(args, "name")?)
            }
            "query_refs" => {
                known_arguments(args, &["name"])?;
                self.query(Verb::Refs, &string_argument(args, "name")?)
            }
            "query_impact" => {
                known_arguments(args, &["name", "depth"])?;
                let depth = depth_argument(args)?;
                self.query(Verb::Impact(depth), &string_argument(args, "name")?)
            }
            // A tool this build does not have is a protocol error, not a tool
            // that failed: nothing ran, so there is no result to report.
            other => {
                let known = TOOL_NAMES.join(", ");
                return Err(invalid_params(format!(
                    "unknown tool `{other}`; known tools: {known}"
                )));
            }
        };
        Ok(match produced {
            Ok(doc) => tool_result(&doc),
            Err(e) => tool_failure(&e),
        })
    }

    /// Ask the stored graph one question about one name.
    ///
    /// The selection rule is [`NameIndex`]'s, unchanged: an exact FQN wins,
    /// otherwise every node the name is a suffix of. Zero and several are both
    /// documents, because choosing between candidates would be the guess this
    /// project forbids everywhere else.
    fn query(&self, verb: Verb, name: &str) -> Result<Value, String> {
        let store = ReadStore::open(&self.db)?;
        let index = NameIndex::build(&store)?;
        let mut found = index.lookup(name);
        if found.matches.len() != 1 {
            let empty = found.matches.is_empty();
            let mut all = found.matches;
            all.extend(found.shadowed);
            return Ok(if empty {
                json::query_no_match(verb.name(), name)
            } else {
                json::query_ambiguous(verb.name(), name, &all)
            });
        }
        let node = found.matches.remove(0);
        // The candidates the exact match won over travel with every answer:
        // a model that reads `status: ok` and one node has been told the
        // name had one reading, and that must be true.
        let shadowed = found.shadowed;
        match verb {
            Verb::Def => {
                let Some(def) = definition(&store, &node.id)? else {
                    // The index was read from the node table under a read-only
                    // open, so this needs the file to have changed underneath.
                    return Err(format!("{}: the node vanished between reads", node.name));
                };
                Ok(json::query_definition(name, &def, &shadowed))
            }
            Verb::Refs => {
                let sites = references(&store, &node.id)?;
                Ok(json::query_references(name, &node, &sites, &shadowed))
            }
            Verb::Impact(depth) => {
                let found = impact(&store, &node.id, depth)?;
                Ok(json::query_impact(name, &node, depth, &found, &shadowed))
            }
        }
    }
}

/// Build or refresh a repository's graph, exactly as `arthron scan` does.
///
/// Same configuration, same default store, same document — a scan started from
/// an agent and one started from a shell must not be able to disagree.
fn scan(root: &Path, db: Option<&Path>) -> Result<Value, String> {
    let config = Config::load(root)?;
    // As on the command line: an explicit path wins and is taken as given,
    // while the scanned repository's own `db` may not name a store outside it.
    let db_path = match db {
        Some(explicit) => explicit.to_path_buf(),
        None => config
            .db_path(root)?
            .unwrap_or_else(|| root.join(".arthron/graph.redb")),
    };
    if let Some(parent) = db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let report = scan_repo_with(root, &db_path, &config)?;
    Ok(json::scan(&report, &config))
}

/// A successful call: the document as text for a model, and again as
/// `structuredContent` for a program. One value rendered twice, never two
/// values that could drift.
fn tool_result(doc: &Value) -> Value {
    match json::render(doc) {
        Ok(text) => json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": doc,
            "isError": false,
        }),
        Err(e) => tool_failure(&e),
    }
}

/// A call that measured nothing. No `structuredContent`: there is no document,
/// and an empty one would read as an empty answer.
fn tool_failure(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

/// A JSON-RPC error response.
fn error(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// The `initialize` result, with the protocol version settled.
fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = match asked {
        Some(v) if COMPATIBLE.contains(&v) => v,
        _ => PROTOCOL_VERSION,
    };
    json!({
        "protocolVersion": version,
        // Tools and nothing else. Declaring a capability this build does not
        // serve would have a client calling a method that answers -32601.
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "arthron", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

/// Every tool this build serves, in the order `--help` lists them.
const TOOL_NAMES: [&str; 4] = ["scan_repo", "query_def", "query_refs", "query_impact"];

/// The `tools/list` payload.
///
/// `additionalProperties: false` is enforced, not decorative: an unknown
/// argument key is refused by name, the way an unknown `arthron.toml` key is.
/// A typo that silently did nothing would be a query answering about something
/// other than what was asked.
fn tools() -> Value {
    json!([
        {
            "name": "scan_repo",
            "title": "Scan a repository",
            "description": concat!(
                "Build or refresh the code graph for a repository and return what it now ",
                "holds: per-language resolved, external, local_binding and unresolved ",
                "counts, every unresolved reason with its count, and the resolution rate. ",
                "Re-running over an unchanged tree re-reads the stored graph. Call this ",
                "before the query tools, which read a graph and never build one."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repository root to scan." },
                    "db": {
                        "type": "string",
                        "description": "Where to write the graph. Default: the repository's \
                                        arthron.toml `db`, else <path>/.arthron/graph.redb.",
                    },
                },
                "required": ["path"],
                "additionalProperties": false,
            },
        },
        {
            "name": "query_def",
            "title": "Find a definition",
            "description": concat!(
                "Where a name is defined. Returns the definition record, every site that ",
                "declares it — two files declaring one identity is a fact, not a ",
                "contradiction — and what it forwards to when it is an alias."
            ),
            "inputSchema": name_schema(),
        },
        {
            "name": "query_refs",
            "title": "Find references",
            "description": concat!(
                "Every stored reference row that resolved to this name: file, line, what ",
                "the site does, the definition it sits in, the literal text at the site, ",
                "and how many times the row occurs."
            ),
            "inputSchema": name_schema(),
        },
        {
            "name": "query_impact",
            "title": "Find what a change reaches",
            "description": concat!(
                "What transitively reaches this name, layer by layer outward, for judging ",
                "the blast radius of a change. Cycle-guarded and depth-bounded; the result ",
                "says whether the bound cut the walk short rather than presenting a ",
                "truncated answer as a complete one."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": NAME_ARGUMENT },
                    "depth": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "How many hops of the reverse closure to walk. \
                                        Default 3.",
                    },
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        },
    ])
}

/// What every query tool's `name` argument accepts. One string, because the
/// rule is one rule and three wordings would eventually be three rules.
const NAME_ARGUMENT: &str = concat!(
    "A full FQN, or any suffix of one that does not cut an identifier in half ",
    "(`Parse`, `#Parse`, `example.com/app/util#Parse`). A suffix several nodes ",
    "end is answered with all of them and status \"ambiguous\"."
);

/// The input schema shared by the two tools that take only a name.
fn name_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "name": { "type": "string", "description": NAME_ARGUMENT } },
        "required": ["name"],
        "additionalProperties": false,
    })
}

/// Refuse an argument key no tool has, naming it and what it could have been.
fn known_arguments(args: &Value, known: &[&str]) -> Result<(), RpcError> {
    let Some(map) = args.as_object() else {
        return Err(invalid_params("`arguments` must be an object".to_string()));
    };
    for key in map.keys() {
        if !known.contains(&key.as_str()) {
            return Err(invalid_params(format!(
                "unknown argument `{key}`; known arguments: {}",
                known.join(", "),
            )));
        }
    }
    Ok(())
}

/// A required string argument.
fn string_argument(args: &Value, key: &str) -> Result<String, RpcError> {
    match args.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(invalid_params(format!("`{key}` is empty"))),
        Some(_) => Err(invalid_params(format!("`{key}` must be a string"))),
        None => Err(invalid_params(format!("`{key}` is required"))),
    }
}

/// An optional string argument. An explicit `null` is the same as absent —
/// clients fill optional fields with it — but a present empty string is a
/// mistake worth naming.
fn optional_string_argument(args: &Value, key: &str) -> Result<Option<String>, RpcError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(Value::String(_)) => Err(invalid_params(format!("`{key}` is empty"))),
        Some(_) => Err(invalid_params(format!("`{key}` must be a string"))),
    }
}

/// The impact walk's depth bound, or its default.
fn depth_argument(args: &Value) -> Result<u32, RpcError> {
    let Some(value) = args.get("depth") else {
        return Ok(DEFAULT_IMPACT_DEPTH);
    };
    if value.is_null() {
        return Ok(DEFAULT_IMPACT_DEPTH);
    }
    let Some(n) = value.as_u64() else {
        return Err(invalid_params(
            "`depth` must be a non-negative integer".to_string(),
        ));
    };
    u32::try_from(n).map_err(|_| invalid_params(format!("`depth` is out of range: {n}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::new(PathBuf::from("/nonexistent/graph.redb"))
    }

    fn request(id: i64, method: &str, params: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
    }

    #[test]
    fn the_handshake_settles_on_a_version_and_declares_only_tools() {
        let reply = server()
            .handle(&request(
                1,
                "initialize",
                json!({ "protocolVersion": "2025-03-26", "capabilities": {} }),
            ))
            .expect("a request is answered");
        assert_eq!(reply["id"], 1);
        let result = &reply["result"];
        // A version this server speaks is echoed back, not overridden.
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], "arthron");
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["capabilities"].get("resources").is_none());
        assert!(result["capabilities"].get("prompts").is_none());
    }

    #[test]
    fn an_unknown_protocol_version_is_answered_with_this_ones() {
        let reply = server()
            .handle(&request(
                1,
                "initialize",
                json!({ "protocolVersion": "1999-01-01" }),
            ))
            .expect("a request is answered");
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn a_notification_is_never_answered() {
        // Not even one this build does not understand: JSON-RPC forbids a
        // response to a message with no id, and a client waiting for one that
        // is never coming is the hang this rule exists to prevent.
        for method in [
            "notifications/initialized",
            "notifications/cancelled",
            "nope",
        ] {
            let line = json!({ "jsonrpc": "2.0", "method": method }).to_string();
            assert_eq!(server().handle(&line), None, "{method}");
        }
    }

    #[test]
    fn blank_framing_is_not_a_message() {
        assert_eq!(server().handle("   \n"), None);
        assert_eq!(server().handle(""), None);
    }

    #[test]
    fn a_line_that_is_not_json_is_a_parse_error_against_a_null_id() {
        let reply = server().handle("{not json").expect("an error is answered");
        assert_eq!(reply["error"]["code"], PARSE_ERROR);
        assert_eq!(reply["id"], Value::Null);
        assert_eq!(reply["jsonrpc"], "2.0");
    }

    #[test]
    fn a_message_that_is_not_an_object_is_an_invalid_request() {
        // Including a JSON-RPC batch, which MCP's current transport removed.
        for line in ["[1, 2]", "\"hello\"", "7"] {
            let reply = server().handle(line).expect("an error is answered");
            assert_eq!(reply["error"]["code"], INVALID_REQUEST, "{line}");
        }
    }

    #[test]
    fn a_request_without_the_version_string_is_refused_with_its_id() {
        let line = json!({ "id": 4, "method": "tools/list" }).to_string();
        let reply = server().handle(&line).expect("an error is answered");
        assert_eq!(reply["id"], 4);
        assert_eq!(reply["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn a_null_id_is_not_an_id() {
        let line = json!({ "jsonrpc": "2.0", "id": null, "method": "tools/list" }).to_string();
        let reply = server().handle(&line).expect("an error is answered");
        assert_eq!(reply["id"], Value::Null);
        assert_eq!(reply["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn an_unknown_method_is_method_not_found_and_names_itself() {
        let reply = server()
            .handle(&request(9, "resources/list", json!({})))
            .expect("a request is answered");
        assert_eq!(reply["id"], 9);
        assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
        assert!(
            reply["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("resources/list")
        );
    }

    #[test]
    fn a_string_id_comes_back_a_string() {
        let line =
            json!({ "jsonrpc": "2.0", "id": "abc", "method": "ping", "params": {} }).to_string();
        let reply = server().handle(&line).expect("a request is answered");
        assert_eq!(reply["id"], "abc");
        assert_eq!(reply["result"], json!({}));
    }

    #[test]
    fn every_tool_is_listed_with_a_schema_and_documented_in_help() {
        let reply = server()
            .handle(&request(2, "tools/list", json!({})))
            .expect("a request is answered");
        let listed = reply["result"]["tools"].as_array().expect("an array");
        let names: Vec<&str> = listed
            .iter()
            .map(|t| t["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(names, TOOL_NAMES);
        for tool in listed {
            let name = tool["name"].as_str().expect("a name");
            assert!(
                tool["description"].as_str().is_some_and(|d| !d.is_empty()),
                "{name} has no description",
            );
            assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
            // Enforced in `known_arguments`, so declaring it is not a claim
            // the server fails to keep.
            assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
            assert!(HELP.contains(name), "{name} is served but not in --help");
        }
    }

    #[test]
    fn tools_list_takes_no_arguments_and_needs_none() {
        // A client that sends no `params` at all must still get the list.
        let line = json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }).to_string();
        let reply = server().handle(&line).expect("a request is answered");
        assert_eq!(
            reply["result"]["tools"].as_array().expect("an array").len(),
            4
        );
    }

    #[test]
    fn an_unknown_tool_is_invalid_params_and_lists_the_real_ones() {
        let reply = server()
            .handle(&request(
                5,
                "tools/call",
                json!({ "name": "query_everything", "arguments": {} }),
            ))
            .expect("a request is answered");
        assert_eq!(reply["error"]["code"], INVALID_PARAMS);
        let message = reply["error"]["message"].as_str().expect("a message");
        assert!(message.contains("query_everything"), "{message}");
        assert!(message.contains("query_def"), "{message}");
    }

    #[test]
    fn a_missing_or_mistyped_argument_never_reaches_the_graph() {
        for arguments in [json!({}), json!({ "name": 7 }), json!({ "name": "" })] {
            let reply = server()
                .handle(&request(
                    6,
                    "tools/call",
                    json!({ "name": "query_def", "arguments": arguments }),
                ))
                .expect("a request is answered");
            assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{arguments}");
        }
    }

    #[test]
    fn an_argument_no_tool_has_is_refused_by_name() {
        let reply = server()
            .handle(&request(
                7,
                "tools/call",
                json!({ "name": "query_def", "arguments": { "nmae": "Parse" } }),
            ))
            .expect("a request is answered");
        assert_eq!(reply["error"]["code"], INVALID_PARAMS);
        assert!(
            reply["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("nmae")
        );
    }

    #[test]
    fn a_store_that_will_not_open_is_a_tool_failure_carrying_the_reason() {
        // Not a JSON-RPC error: the call was well-formed. Not a document
        // either — nothing was measured, so there is no result to report.
        let reply = server()
            .handle(&request(
                8,
                "tools/call",
                json!({ "name": "query_refs", "arguments": { "name": "Parse" } }),
            ))
            .expect("a request is answered");
        let result = &reply["result"];
        assert_eq!(result["isError"], true);
        assert!(result.get("structuredContent").is_none());
        assert!(
            !result["content"][0]["text"]
                .as_str()
                .expect("a message")
                .is_empty()
        );
    }

    #[test]
    fn depth_defaults_and_refuses_what_it_cannot_walk() {
        assert_eq!(depth_argument(&json!({})).ok(), Some(DEFAULT_IMPACT_DEPTH));
        assert_eq!(
            depth_argument(&json!({ "depth": null })).ok(),
            Some(DEFAULT_IMPACT_DEPTH)
        );
        assert_eq!(depth_argument(&json!({ "depth": 1 })).ok(), Some(1));
        for bad in [
            json!({ "depth": -1 }),
            json!({ "depth": 1.5 }),
            json!({ "depth": "3" }),
        ] {
            assert!(depth_argument(&bad).is_err(), "{bad}");
        }
        assert!(depth_argument(&json!({ "depth": u64::MAX })).is_err());
    }
}
