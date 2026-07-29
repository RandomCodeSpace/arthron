# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Pre-1.0, the public API and the CLI surface may change in any release.

Decisions and their rationale — including what was rejected — live in
[`docs/decisions.md`](docs/decisions.md); this file records what shipped.

## [Unreleased]

### Changed

- **One `LocalBinding` rule in every tier-1 track, and Go emits type uses — a
  deliberate re-base of seven baselines.** The ratified rule is that a
  reference whose root is a parameter or a local variable names a thing that is
  not a node by decision, so it is reported beside `external` and excluded from
  **both** terms of the resolution rate. Go, TypeScript and JavaScript already
  read it that way; Java and Python applied it only when the *whole* target was
  the bound name, so `f.m()` sat outside both rate terms in Go and inside them
  in Java, and the two rates were computed over differently-sized denominators.

  A **receiver is not a local**, and that half of the rule went the other way.
  Go has no `this` keyword — the receiver is the name a method uses to reach
  its own value — and Go alone filed a member selected through it as a local
  binding. Java, Python, JavaScript and TypeScript all resolve `this.m()` /
  `self.m()` by declared-type lookup and count it in both rate terms, so the
  commonest shape in object-oriented code sat outside Go's denominator and
  inside everyone else's. Go now resolves `t.m()` against the receiver type its
  own signature states, which is the strongest declared-type evidence any of
  the five gives; a member the receiver type does not itself declare is
  `NeedsReceiverType` rather than `NoMatchingDefinition`, because this track
  indexes neither Go embedding nor struct fields and the lookup table is
  therefore not complete.

  Separately, the Go extractor emitted only calls and imports, so "tier 1: call
  sites, imports and type uses" was not true of Go; `ref-type` now emits a
  reference for every written type position. Baselines are re-based, not
  compared, because what is *in* the rate's terms changed. Measured, release
  build, cold store:

  | baseline | resolved | unresolved | external | local_binding | rate |
  |---|---:|---:|---:|---:|---:|
  | `go-codeiq` | 4,467 → 8,016 | 799 → 884 | 6,085 → 12,210 | 4,276 → 4,113 | 84.8% → **90.1%** |
  | `go-caddy` | 3,006 → 10,208 | 1,815 → 2,700 | 9,571 → 19,201 | 9,425 → 8,252 | 62.4% → **79.1%** |
  | `go-probes` | 17 | 0 | 0 → 26 | 1 | 100.0% |
  | `java-commons-lang` | 39,591 → 34,217 | 19,093 → 16,279 | 68,297 → 63,385 | 2,062 → 15,162 | 67.5% → **67.8%** |
  | `java-gson` | 16,074 → 12,885 | 7,215 → 6,105 | 18,187 → 16,737 | 957 → 6,706 | 69.0% → **67.9%** |
  | `python-django` | 19,103 | 13,764 → 6,185 | 13,326 | 826 → 8,405 | 58.1% → **75.5%** |
  | `python-flask` | 1,192 → 1,185 | 2,847 → 877 | 2,336 → 2,317 | 150 → 2,146 | 29.5% → **57.5%** |

  The other eighteen baselines are byte-identical, including both TypeScript
  and both JavaScript corpora and all fourteen tier-2 baselines, whose
  `local_binding` is still zero.

  **Attributed per reference, not inferred from the totals.** In Java and
  Python not one reference was added or removed and every reference that
  changed outcome moved *into* `local_binding` — 13,100 on commons-lang (5,374
  from `resolved`, 4,912 from `external`, 2,814 from an unresolved reason),
  5,749 on gson, 7,579 on django, 1,996 on flask — and nothing moved in any
  other direction. In Go two things moved rows and they are separable. Every
  *added* occurrence is a type use, 9,596 on codeiq and 16,544 on caddy, all of
  kind `type-use`, of which 3,439 and 6,732 resolve to a definition that had no
  row at all before. Every *pre-existing* occurrence that changed its answer is
  rooted at a method receiver — 195 on codeiq and 1,349 on caddy, counted at
  the extractor by re-rooting — and every one of them left `local_binding`, 110
  and 470 of them for `resolved` and the rest for `NeedsTypeInference` (the
  `t.a.b()` shape, whose real receiver is the field `a`) or `NeedsReceiverType`
  (3 and 123). No other Go occurrence changed its outcome or its count. The
  changes do not touch each other's languages, measured by re-running each
  corpus with the other reverted.

  **A rate that rises here is not an improvement.** Excluding a class from both
  terms is exactly how a rate rises with nothing linked better, and Python's
  does: django's `NeedsTypeInference` falls 10,256 → 2,677 and flask's 2,119 →
  186 because those references are now `local_binding`, not because any of them
  reached a definition. The `local_binding` column is gated for drift for this
  reason and a re-base has to state it.

  **And what it costs is edges.** The 5,374 commons-lang and 3,189 gson
  occurrences that moved from `resolved` to `local_binding` are 13.6% and 19.8%
  of those corpora's resolved edges, and they are gone from the graph
  `arthron query` reads and the MCP server serves — for many of them the
  resolver had already produced the `NodeId`, and the store still holds the
  node. `LocalBinding` does not claim the target is unnameable; it claims the
  reference is not evidence about cross-file linking, because reaching its
  target needs the type of a binding no other file can see. Python loses
  almost nothing this way: django 0 and flask 7.

- **`arthron scan` prints the rate's denominator under every language line.**
  `rate denominator 8900 of 25223 references (35.3%)`: `(resolved +
  unresolved)` over every reference the language emitted. Excluding `external`
  and `local_binding` from both of the rate's terms is correct and it also
  makes the denominator a fraction of the surface — codeiq's Go rate of 90.1%
  covers 35.3% of Go's references, fastify's 63.0% covers 14.2% of
  JavaScript's — and a rate published without its share reads as a claim about
  the whole. Text report only, on `scan` and on `gate`. **`--json` is
  unchanged and its `schema` does not move**: the document already carries all
  four counts, so a consumer derives the share exactly, and a field for
  arithmetic is not a field.

- **The tier-1 claim is retracted to what is measured.** The README, the
  changelog and the report line called tier 1 "call-graph resolution". It is
  not: method dispatch mostly does not resolve, and `NeedsTypeInference` — 758
  of codeiq's 884 unresolved rows — is most of what tier 1 leaves unlinked in
  all five languages. A call through a receiver whose type its own signature
  states does resolve, in all five, since the locals re-base above. Tier 1 now
  reads "definitions, references, and cross-file import and function-call
  resolution", and the scan line prints `tier 1: call, import and type-use
  resolution` — which is what the denominator holds. Nothing measured changed;
  no baseline moved.

### Fixed

- **Two Go definition defects the new type-use surface exposed.** `def-type`
  read only `type_spec`, so a package-level `type X = Y` declared no node —
  free while Go emitted no type uses, and 57 codeiq / 7 caddy
  `NoMatchingDefinition` rows the moment it did; it now reads `type_alias` too,
  which is the whole of the `DefKind::Type` census moving 229 → 232 on codeiq
  and 507 → 511 on caddy. And `case nil:` in a type switch is a
  `type_identifier` in this grammar, now answered from the predeclared block
  rather than left unmatched. After both, `NoMatchingDefinition` is 123 on
  codeiq and 269 on caddy — unchanged from before the wave.
- **`Generic[T](x)` was reported as a type use naming a function, and as no
  call at all.** An explicit instantiation is unambiguously a type, so the Go
  grammar files the whole call as a `type_conversion_expression` over a
  `generic_type` and the `call_expression` rule never saw it; the only row the
  site produced was a `TypeUse` whose target was a `DefKind::Function`. The
  callee position of a call written in call syntax is now reported as the
  `Call` it is — still exactly one row for one site, and the type *arguments*
  are unaffected. No published number moves: there is no explicit
  instantiation in `codeiq`, `caddy` or `probes`, counted syntactically over
  all 728 files.
- **Two Java external nodes that claimed a package which does not exist.**
  `Outer.NonStaticInner` and `Enclosing<T>.Inner` in gson's `TypeTokenTest`
  name method-local classes (JLS §14.3); their two-segment targets escaped the
  narrow local rule and were filed as `External("Outer")` and
  `External("Enclosing")`. gson's stored external census is 36 → 34.

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
- **Documented exit code 2 was narrower than the code.** Every place that
  described it — the README table, `--json`'s help, the module docs, `gate
  --help` — said "nothing was measured: usage, I/O or the environment, safe to
  retry". `gate` also returns 2 for `GateVerdict::Error`, a baseline or a run
  whose `resolved + unresolved` is zero: measured, deterministic, and not
  worth retrying. The exit code is unchanged and the documentation now says
  both halves and which is which.
- **`gate --help` contradicted itself about `db`.** It refused the config's
  `db` key on the grounds that "a gate is only meaningful against a cold
  store", and then documented a `--db` flag that is honoured as given —
  including at a store that already holds a graph, which is re-scanned warm.
  The real reason the config key is ignored is that where the run writes is
  not the scanned repository's decision; `--db` is yours, warm store and all,
  which is why the default is a fresh temporary one.
- **`arthron mcp --help` stated the wrong default for `scan_repo`'s `db`.** It
  said `<path>/.arthron/graph.redb`, omitting that the scanned repository's
  `arthron.toml` `db` wins first — which the tool's own JSON schema already
  said correctly. The two now agree.
- **The `--db` cwd-versus-config-root asymmetry is written down.** A config's
  `db` is resolved against the repository it sits in; `--db` is resolved
  against the current working directory, so `arthron scan ./repo --db
  graph.redb` writes `./graph.redb`. Documented on `scan --db`, on `gate
  --db`, and in the README's `arthron.toml` section. Behaviour unchanged.
- **`CONTEXT.md` defined an edge as a resolved reference only.** An `External`
  reference produces an edge too, to the dependency node it reached — that is
  what makes `query impact` see a call into a dependency instead of a dead
  end. The glossary entry now says `Resolved` **or** `External`, and that
  `Unresolved` produces none.
- **Two comments still described the tier-2 tracks as disabled** — `src/lib.rs`
  and the `REGISTRY` list — from before all fourteen went live. Comments only.
- The 0.0.1 changelog omitted the Windows baseline round-trip fix; it is now
  recorded under that release, where it shipped.
- The kubernetes cold-scan RSS that failed the hard gate is quoted as its
  measured value, **729.1 MiB**, everywhere it appears. It was rounded to
  729.0 in the summaries downstream of the benchmark that measured it.

## [0.0.1] - 2026-07-28

First release with an engine in it. `arthron scan` builds a real cross-file
graph, `arthron gate` blocks a resolution-rate regression, and `arthron query`
and `arthron mcp` answer questions about the result.

### Added

- **Nineteen live languages, tiered and declared.** Five at **tier 1** —
  definitions, references, and cross-file import and function-call resolution,
  the rate taken over call sites, imports and type uses together: Go, Java,
  JavaScript, TypeScript, Python. Tier 1 is which reference kinds are in the
  denominator, not a claim that the call graph is complete; method dispatch
  through a value whose type must be inferred is `NeedsTypeInference` and is
  most of what tier 1 leaves unlinked. Fourteen at **tier 2** — definitions,
  structure and imports, with no verified call edges, so the rate is an
  import-resolution rate: C++, C#, Kotlin, Swift, Ruby, PHP, Rust, Scala,
  Dart, Elixir, Haskell, Lua, Bash, HCL. Each language family is its own
  identity domain; JavaScript and TypeScript deliberately share one, and
  Kotlin/Scala deliberately do not share Java's.
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
  scan fell from **729.1 MiB to 337.1 MiB** — 66% of the hard 512 MB ceiling on
  the 2 vCPU reference hardware, six runs spanning 0.2% — by committing every
  phase per 500 files, capping the redb page cache on both open paths, and
  having phase 2 consume extracted facts per file instead of borrowing the whole
  set. That percentage reads ceiling and measurement in the same binary units,
  337.1 of 512 MiB; it is the basis the failing 729.1 was recorded against as
  1.42× the ceiling, and reading `512 MB` as decimal MB instead makes the same
  measurement 353.5 MB, 69%. No timing regression (~17 s per 1M lines against a
  60 s target). Graph identity was proven byte-for-byte across five corpora at
  the level of full blake3 snapshot digests, not matching tallies.

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
- **A baseline re-based on Windows could not be read back.** `--rebase` wrote
  the corpus path as the platform spells it, so on Windows the `corpus` field
  came out `corpus\go\codeiq`. The baseline format has no escapes, and its
  reader rejects a `\` for exactly that reason — so the gate wrote a file no
  later gate run could parse, and the next run failed as a usage error on a
  baseline it had just produced itself. The path is now written `/`-separated
  on every platform, the way the repo-relative keys in the store already are,
  and `\` joined `"` and newline in the set of characters a provenance field
  is refused for before anything is written rather than after.

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
