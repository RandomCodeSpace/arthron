# The MCP server

`arthron mcp` serves the stored graph over the [Model Context
Protocol](https://modelcontextprotocol.io) so an agent can ask what a name is,
where it is used, and what a change to it reaches.

It is a transport and nothing else. Every tool returns the same JSON document
`--json` prints, built by the same library calls the command line makes. There
is no second answer for agents: a number that is wrong here is wrong in the
terminal too, which is the only arrangement in which two surfaces stay honest
about one graph.

## Running it

```bash
arthron mcp [--db <FILE>]
```

Transport is **stdio**: JSON-RPC 2.0, one message per line, stdin in and stdout
out. No `Content-Length` framing, no socket, no address bound — here or
anywhere else in this binary. Nothing about your code leaves the machine.

The three query tools read one graph for the whole session, chosen once at
startup:

1. `--db` if given,
2. else the `db` key in `arthron.toml` in the working directory,
3. else `.arthron/graph.redb` in the working directory.

`scan_repo` writes wherever its own arguments say, so a session can index a
repository and then query whatever it just indexed — start the server in the
repository root and the defaults line up.

Configured in a client that speaks stdio MCP:

```json
{
  "mcpServers": {
    "arthron": {
      "command": "arthron",
      "args": ["mcp"],
      "cwd": "/path/to/your/repo"
    }
  }
}
```

## The four tools

| Tool | Arguments | Answers |
|---|---|---|
| `scan_repo` | `path` (required), `db` | Build or refresh a repository's graph; return the per-language counts, every unresolved reason, and the rate. |
| `query_def` | `name` | The definition record, every site that declares it, and what it forwards to when it is an alias. |
| `query_refs` | `name` | Every stored reference row that resolved to the name. |
| `query_impact` | `name`, `depth` (default 3) | What transitively reaches the name, layer by layer, and whether the depth bound cut the walk short. |

A `name` is a full FQN or any suffix of one that does not cut an identifier in
half: `example.com/app/util#Parse`, `#Parse` and `Parse` all select that node.
An exact FQN wins outright and is never widened into a suffix search.

The query tools read a graph and never build one. Call `scan_repo` first.

Every field of every returned document is described by `arthron scan --json
--help`; the shape is versioned by the document's `schema` field and does not
move when a field is added.

## What is an answer and what is an error

The same split the `--json` surface makes, for the same reason.

- A name that matches **nothing**, or matches **several** nodes, is an
  *answer*. `isError` is false and the document carries `status` `"no_match"`
  or `"ambiguous"` — with every candidate listed, because choosing between them
  would be exactly the guess this project's resolver is forbidden to make. The
  model reads the list and asks again with a full FQN.
- A store that will not open, or a scan that fails, measured nothing. That is
  `isError: true` with the reason as text and **no** `structuredContent`: an
  empty document would read as an empty answer.
- Arguments a tool cannot take never reach the graph. They come back as
  JSON-RPC `-32602`, naming what was wrong. An argument key no tool has is
  refused by name, the way an unknown `arthron.toml` key is.

A successful call carries the document twice: pretty-printed as text content,
for a model to read, and again as `structuredContent`, for a program to parse.
Both are rendered from one value and cannot drift.

## Protocol details

- **Methods:** `initialize`, `tools/list`, `tools/call`, `ping`, and the
  `notifications/*` a client sends after initialising.
- **Capabilities:** `tools` only. Declaring one this build does not serve would
  have a client calling a method that answers `-32601`.
- **Versions:** `2025-06-18`, `2025-03-26` and `2024-11-05` are all echoed back
  if asked for — the three define this surface identically. Anything else is
  answered with `2025-06-18`, and the client decides whether to continue.
- **Nothing a client sends ends the session.** A line that is not UTF-8, is not
  JSON, or is not a JSON-RPC request is answered with a JSON-RPC error and the
  server reads the next line. Only end of input stops it.
- **A notification is never answered** — not even one this build does not
  understand. JSON-RPC forbids responding to a message with no `id`, and a
  client waiting for a response that is never coming is the hang that rule
  exists to prevent. A *request* naming an unknown method gets `-32601`.
- **Batching is not supported**, which is also what the current MCP transport
  says. A message that is not a JSON object is `-32600`.
