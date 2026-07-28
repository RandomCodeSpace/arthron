# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Pre-1.0, the public API and the CLI surface may change in any release.

Decisions and their rationale — including what was rejected — live in
[`docs/decisions.md`](docs/decisions.md); this file records what shipped.

## [Unreleased]

### Fixed

- **A repository's `db` may no longer name a store outside the tree through a
  link with nothing on the other end.** The containment check canonicalises the
  deepest existing component of the resolved `db` path and asks whether it is
  under the root. A dangling symbolic link answers `lstat` and fails
  `canonicalize`, and that failure read as "not there yet", so the walk stepped
  past the link to its parent — inside the root — and called the whole path
  contained. The store was then created *through* the link: one arbitrary file,
  anywhere the process could write, from a scanned repository's own
  `arthron.toml`, at exit 0 with nothing said. A component that exists and does
  not resolve is now refused, because it cannot be shown to stay inside the
  root. Reachable from `arthron scan` and from the MCP `scan` tool.
- **A scan of a root that is not there answers 2 whatever `--db` says.**
  Creating the store's directory ran first, and the default store lives at
  `<root>/.arthron/graph.redb` — so `create_dir_all` made the missing root, the
  walk found the empty tree it had just made, and the run answered 0 with a
  report of zeros. With `--db` elsewhere the same invocation already answered 2,
  so the exit code depended on where the store happened to sit. The root is now
  checked before anything is created.
- **A track whose project layout it cannot read reports no tally for its
  language.** The rows an earlier scan wrote stay in the store — a track that
  cannot read the layout is in no position to say which files are gone — but
  `scan` no longer prints their tally beside the line saying the track measured
  nothing, and `gate --db <persistent store>` no longer re-bases a baseline onto
  numbers this run did not produce.
- A symbolic link out of the tree is now named as such whatever is on the other
  end of it; the message no longer says "definitions" about a directory.

## [0.0.1] - 2026-07-28

First release with an engine in it. `arthron scan` builds a real cross-file
graph, `arthron gate` blocks a resolution-rate regression, and `arthron query`
and `arthron mcp` answer questions about the result.

### Added

- **Nineteen live languages, tiered and declared.** Five at **tier 1** —
  definitions, references and call-graph resolution: Go, Java, JavaScript,
  TypeScript, Python. Fourteen at **tier 2** — definitions, structure and
  imports, with no verified call edges, so the rate is an import-resolution
  rate: C++, C#, Kotlin, Swift, Ruby, PHP, Rust, Scala, Dart, Elixir, Haskell,
  Lua, Bash, HCL. Each language family is its own identity domain; JavaScript
  and TypeScript deliberately share one, and Kotlin/Scala deliberately do not
  share Java's.
- **The three-outcome resolution contract.** Extractors emit references and
  never edges; one resolver owns all linking; every reference ends as
  `Resolved`, `External`, or `Unresolved` carrying a reason. There is no way to
  express "dropped". `External` and `LocalBinding` sit outside both terms of the
  resolution rate and are gated on drift so neither can inflate it.
- **`arthron gate` and twenty-five committed baselines** in `baselines/`, one
  per corpus, measured on a release build against a pinned snapshot. Every
  baseline is bound to a test driver *and* to a step in the corpus-gate
  workflow, and `tests/baselines.rs` fails the build if either is missing — it
  reads no corpus, so it is enforcement that runs on every platform. Exit 0
  pass, 1 regression, 2
  usage or I/O error; `--rebase` is the only way the ratchet moves. The gate
  fails with `denominator_shrank` when `resolved + unresolved` falls below the
  baseline's, so a dropped row cannot read as an improvement.
- **Corpus gates in CI** (`.github/workflows/gate.yml`), the one job that
  fetches the private corpus and the place a red check blocks a merge —
  twenty-five steps, one per committed baseline, each naming its corpus in the
  step list so a regression is identified without reading a log.
- **`arthron query def | refs | impact`** — a definition and its declaring
  sites, every stored reference row that resolved to a name, and the reverse
  transitive closure (`--depth`) that gives a change's blast radius. Names
  resolve by full FQN or any suffix starting at a separator; an ambiguous
  suffix returns every candidate and exit code 1 rather than guessing. The
  store is opened read-only.
- **`--json` on every command**, one document per run, from the same library
  calls the human-readable report uses.
- **`arthron.toml`** — optional `include` / `exclude` globs compiled straight
  into the file walk, an optional `db` path, and a `[tracks]` table that can
  switch a live track off but can never switch on a track the binary does not
  implement. An unrecognised key is refused by name rather than ignored.
- **`arthron mcp`** — the graph served to an agent over the Model Context
  Protocol on stdio: JSON-RPC 2.0, one message per line, `scan_repo`,
  `query_def`, `query_refs`, `query_impact`. No socket is opened and no address
  is bound.
- **Incremental re-scan** driven by content hashes: the changed set is exactly
  the files whose hash moved, and a cold index is the same code path with a
  changed set of everything — there is no separate incremental mode that can
  silently skip work.

### Changed

- **Cold-scan memory is bounded.** Peak RSS on a 1,789,247-line kubernetes
  scan fell from **729.0 MiB to 337.1 MiB** — 66% of the hard 512 MB ceiling on
  the 2 vCPU reference hardware, six runs spanning 0.2% — by committing every
  phase per 500 files, capping the redb page cache on both open paths, and
  having phase 2 consume extracted facts per file instead of borrowing the whole
  set. No timing regression (~17 s per 1M lines against a 60 s target). Graph
  identity was proven byte-for-byte across five corpora at the level of full
  blake3 snapshot digests, not matching tallies.

### Fixed

- **A held store is refused by name instead of deadlocking.** Both open paths
  fail fast when another process holds the store, rather than waiting.
- **Unreadable and undecodable files are reported per file** — never dropped,
  never fatal — and a stepped-over file loses its currency claim so the next
  scan re-reads it. An absent scan root fails cleanly.
- **The text report and `--json` list the same languages.** `arthron scan`
  printed a `go` line on every repository, including ones containing no Go,
  which contradicted the documented `--json` contract that a language with no
  rows has no entry and is not a rate of zero. A scan that produced no
  reference row at all now says so in one line instead of naming a language to
  fill the space.

### Packaging

- Crate metadata for crates.io: description, license, repository, keywords,
  categories, and an explicit `include` allowlist. The published crate carries
  `src/` (including the `src/rules/*.yml` files the extractors `include_str!`),
  `Cargo.toml`, `README.md`, `CHANGELOG.md` and `LICENSE` — and not the corpus,
  the baselines, the tests, the docs or the CI configuration.

## [0.0.0] - 2026-07-26

### Added

- Name reservation on crates.io. No engine, no commands — the crate existed so
  that the name would be available when there was something to publish under it.

[Unreleased]: https://github.com/RandomCodeSpace/arthron/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/RandomCodeSpace/arthron/releases/tag/v0.0.1
