# arthron

**ἄρθρον** — *joint*. In anatomy, the articulation where two separate bones meet
and move as one. In Greek grammar, the *article*: the small word whose only job
is binding a reference to its referent.

A local-first code intelligence engine. It parses each file in isolation, then
**resolves** the references between them into a verified graph — and tells you
precisely which references it could not resolve, and why.

> **Status: early.** `arthron scan` works and produces a real graph for Go.
> Everything else in this README that is not marked as working is a target, not
> a promise. The `0.0.0` crate on crates.io predates this code and is a name
> placeholder — build from source.

## What it looks like

```console
$ arthron scan ./my-go-service
go           resolved 2        external 7        unresolved 1        rate 66.7%
             NeedsTypeInference 1
```

Every number there is checkable. In the module that produced it: **2 resolved**
— one in-repo import and the `store.Lookup` call that crosses between packages.
**7 external** — two dependency imports, `fmt.Errorf`, `uuid.New`, and the
`make`/`append`/`len` builtins. **1 unresolved** — a `log.Warn(err)` call on an
interface-typed parameter, which needs type inference the engine does not yet
do. That last one is the point: it is *reported*, not quietly omitted.

## Why it is built this way

The usual way to build a code graph is to let a per-file analyzer emit edges. A
file-local analyzer cannot know whether the symbol it just saw is defined
somewhere else in the repository. So it either guesses — and the guess gets
discarded by whatever validates edges downstream — or it gives up and emits
nothing.

Both paths produce the same result: a graph with plenty of nodes, almost no
edges between files, and a tool that reports success throughout. You cannot ask
it what a change breaks, because it never linked anything to begin with.

arthron inverts the responsibility. **Extractors are forbidden from emitting
edges.** They emit *references* — "this call site names `parseConfig`, in this
scope, at this byte range." A single resolver, which sees every file, owns all
linking, and every reference lands in exactly one of three outcomes:

| Outcome | Meaning |
|---|---|
| `Resolved` | Linked to a definition in this repository. Verified. |
| `External` | Linked to a dependency outside it. |
| `Unresolved` | Could not be linked — **recorded with a reason, never dropped.** |

There is no fourth outcome and no way to express "dropped." An unresolved
reference is data: aggregated, the reasons tell you exactly where language
support is thin.

That makes **resolution rate** — `resolved / (resolved + unresolved)`, per
language, never averaged across them — the number that matters, ranked above
performance. Optimising a tool that resolves nothing is optimising the wrong
thing.

## Install

```bash
git clone https://github.com/RandomCodeSpace/arthron
cd arthron
cargo build --release      # requires Rust 1.89+
./target/release/arthron scan /path/to/repo
```

`cargo install arthron` does **not** work yet — the published crate is a
placeholder from before the engine existed.

## Usage

```
arthron scan <PATH> [--db <FILE>]
```

Builds or refreshes the graph for a repository and prints per-language
resolution rates with a breakdown of every unresolved reason. The graph is
stored at `<PATH>/.arthron/graph.redb` unless `--db` says otherwise.

Re-running against an unchanged tree re-reads the stored graph instead of
re-analysing: the changed set is exactly the files whose content hash moved.
Cold indexing is the same code path with a changed set of everything, so there
is no separate incremental mode that can silently skip work.

```
arthron mcp [--db <FILE>]
```

Serves the graph to an agent over the Model Context Protocol on stdin/stdout —
`scan_repo`, `query_def`, `query_refs` and `query_impact`, each returning the
same JSON document the command line prints. Still no network: stdio only, no
socket bound. See [`docs/mcp.md`](docs/mcp.md).

## What works today

| | State |
|---|---|
| `arthron scan` | **Working** — Go |
| Go resolution | **Working** — definitions, imports, calls, per-reason unresolved reporting |
| Incremental re-scan | **Working** — content-hash changed set |
| Java · JavaScript · TypeScript · Python | Planned, in that order |
| `arthron gate` (CI, fails on regression) | Planned |
| `arthron impact <path>` (blast radius) | Planned |
| `arthron mcp` (MCP server, stdio) | **Working** — [`docs/mcp.md`](docs/mcp.md) |
| `arthron watch` · daemon | Planned |

Language *coverage* spans the 27 languages ast-grep's built-in parsers handle.
Language *capability* is tiered and declared rather than assumed:

- **Tier 1** — definitions, references, and call-graph resolution. Go today;
  Java, JavaScript, TypeScript and Python next.
- **Tier 2** — definitions and file structure, no verified call edges.

Tier 2 is honest rather than degraded: you get symbols and imports, you do not
get call edges, and the tool says which is which instead of inventing the
difference.

## Design targets

Not yet met, and stated so they can be measured against:

| Budget | Target |
|---|---|
| Cold index, 5M LOC | < 30 s |
| Warm re-index, unchanged tree | < 3 s |
| Peak RSS | < 512 MB |
| Reference hardware | **2 vCPU** — a CI runner, not a workstation |

The 2 vCPU floor is deliberate. This runs in CI and inside agent loops, where
being predictable matters more than being fast on a developer's laptop. Resource
ceilings are hard limits; timings are targets.

## Shape

```
ast-grep ──▶ Extractor ──▶ Resolver ──▶ Store
 parse +      YAML rules:   refs →       redb
 match        defs, refs    outcomes
```

One Rust binary. No network calls, ever — nothing about your code leaves the
machine.

## Contributing

Decisions and the reasoning behind them are recorded in
[`docs/decisions.md`](docs/decisions.md), newest first, including what was
rejected. Read the relevant entry before changing something it covers; if you
change it anyway, add an entry saying why.

Measurements must be executed, never estimated. A benchmark that turns out to
be invalid stays in the record with the reason it was wrong.

## License

MIT. See [LICENSE](LICENSE).
