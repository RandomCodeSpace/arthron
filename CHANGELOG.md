# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Pre-1.0, the public API and the CLI surface may change in any release.

Decisions and their rationale — including what was rejected — live in
[`docs/decisions.md`](docs/decisions.md); this file records what shipped.

## [Unreleased]

### Changed

- **The EcmaScript universe scope has a second half, and it un-pollutes
  `NoMatchingDefinition`.** That reason's contract says the reference was
  understood, the lookup table was complete, and the name is absent — "in a
  corpus that compiles this should mean *our* bug, and should sit near zero."
  Both halves were false. All 1,728 occurrences on express were five names —
  `it` 1,111, `describe` 554, `before` 59, `after` 3, `XMLHttpRequest` 1 — and
  13,833 of vue-core's 15,276 were eight more: `expect` 9,930, `test` 2,860,
  `it` 609, `describe` 373, `beforeEach` 37, `afterEach` 21, `afterAll` 2,
  `beforeAll` 1. They are what a test runner puts in the global scope of the
  files it runs, reaching the file with no import because the runner injects
  them.

  The universe scope now models both provenances. The *host*'s half is
  unchanged: a name ECMA-262, Node or the web platform declares is `External`,
  because the thing on the other end genuinely exists. The new half is a
  **package**'s: a name a declared dependency injects is
  `Unresolved(UnknownPackage)`, filed against the package the definition is
  actually in. Six environments are recognised (mocha, jasmine, jest,
  vitest, cypress, qunit), each by its *documented* global set in full rather
  than by the subset that looked unlikely to collide: what makes `it` mocha's
  is not its spelling but the project declaring mocha, checked per file against
  `package.json` and `tsconfig.json`'s `compilerOptions.types`. A repository
  that declares no runner still gets `NoMatchingDefinition` for `describe`, and
  any declaration or import of the name wins, because the universe scope is
  consulted last.

  **No rate moves and nothing is reclassified into `External`,** which is the
  point: `UnknownPackage` is `Unresolved`, so every one of these references
  stays in both terms. express `NoMatchingDefinition` 1,728 → 0 with the rate
  at 28.99% before and after; vue-core 15,276 → 1,443 with the rate at 48.48%
  before and after (13,833 from `NoMatchingDefinition` and 785 from
  `NeedsTypeInference`, the latter being `vi.fn` and `expect.any`, where the
  head decides exactly as it already does for `console.log`). fastify does not
  move at all — it injects nothing. The one external that did move is
  `XMLHttpRequest`, absent from the host list while `WebSocket`,
  `AbortController` and `fetch` were all in it: express `external` 701 → 702,
  the same shape of omission as `Error`'s and worth one row.

  **The written-out import of the same name does not always agree, and the two
  channels differ.** An environment turns on through either `package.json` or
  `tsconfig.json`'s `compilerOptions.types`. Through `types` the two spellings
  match exactly: zod states `"types": ["vitest"]` and declares no dependencies
  at all, so its 168 `from "vitest"` specifiers, the names bound by them and
  the injected spellings all report `UnknownPackage`. Through `package.json`
  they do not: a declared dependency is the dependency boundary, so the import
  and every name reached through it answer `External("npm:<pkg>")`. vue-core
  declares vitest in its root `devDependencies` and carries both — 95
  occurrences of vitest's names under `External`, written as imports, against
  14,618 injected under `Unresolved(UnknownPackage)`. The injected half is not
  the one that may move: `Unresolved` keeps it in both terms of the rate, where
  `External` would take it out of both and raise the gate without linking
  anything. Closing the asymmetry means moving the *imported* side, which is a
  change to what `External` means at the dependency boundary for every package
  and is not made here. Both halves are pinned by a test so neither drifts in
  silence.

- **TypeScript's `compilerOptions.customConditions` is read, and it is the
  largest single miss on zod.** The condition set handed to NODE
  `PACKAGE_TARGET_RESOLVE` was hardcoded per dialect and module kind, so a
  monorepo that publishes built artefacts and points its own compilation at the
  sources instead — `"@zod/source"` written ahead of `"types"` in the same
  `exports` entry, and named in `packages/zod/tsconfig.json` — took the
  `"types"` branch. That branch names an `index.d.cts` beside the manifest that
  no scan of the sources can see, so every self-import missed, and every name
  reached through one missed with it. The option is now read (flattened through
  `extends`, folded into the config fence) and added to the set for the nearest
  TypeScript project, for both `exports` and `imports` maps. It is a *set*, not
  a priority list: NODE matches conditions in the map's own key order, so a
  custom condition can only make a branch reachable that was unreachable, and
  which branch wins stays the package author's decision.

  zod: `ModuleNotFound` 7,822 → 1, `resolved` 10,043 → 17,080, rate 27.24% →
  **46.33%** (+19.09 points), every one of the 7,037 new edges landing in
  `packages/zod/src/`. `NoMatchingDefinition` 524 → 1,123 and
  `NeedsTypeInference` 1,576 → 1,761 in the same movement, and neither is a new
  miss: a module that could not be found gave every name reached through it one
  answer, and a module that *is* found gives each of them its own — including,
  for a namespace re-exported by name, an honest miss this resolver does not
  yet follow. No other corpus states a `customConditions` and none of the other
  three moved a row.

  `extends` is nearest-wins on *stated*, not on non-empty. `"types": []` is the
  documented way to say "no ambient type packages" and `"customConditions": []`
  says the same about conditions; both were read as "unstated" and silently
  given the base's value back, turning an ambient environment on under a config
  that turned it off and sending an import down a branch `tsc` would not take.
  Presence of the key now decides. No corpus states either as an empty list, so
  no gated number moves — verified by a whole-row join against the previous
  scan on all four, which changed nothing.

  **Every changed row on all four corpora was joined whole-row against the
  previous scan.** The row-key set is byte-identical — nothing added, nothing
  removed — and **no reference that already resolved changed its target or its
  outcome**, on any corpus. The 2,194 rows that changed on zod all came out of
  `ModuleNotFound`, the 187 on express and 800 on vue-core all out of the
  ambient class above, and fastify changed nothing.

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

### Fixed

- **A mixed `.js`/`.ts` tree reported a higher JavaScript rate the second time
  it was scanned.** The track runs two passes over one store, and the wake set
  each computes is filtered to the files that pass owns. So when the TypeScript
  pass declared an identity a JavaScript row had already probed and missed — a
  workspace member whose entry point is a `.ts` file is the shape that does it
  — applying it withdrew that file's currency claim and no pass in that scan
  could give it back. The scan ended with the claim outstanding, the *next*
  scan re-read exactly those files, and they resolved against a store that by
  then held the TypeScript definitions. On a two-package fixture the JavaScript
  rate is 0% cold and 100% warm, for a tree nobody touched. A rate that depends
  on how many times it has been measured is not a measurement, and the cold
  number is the one every baseline is taken from. The track now runs JavaScript
  once more to converge; that pass's changed set is exactly the files whose
  claims are outstanding, which is empty in the ordinary case, and it
  terminates because a module's identity here is its path. Measured cost, best
  of three interleaved cold runs: express +0.03 s, fastify +0.01 s, vue-core
  +0.09 s, zod within noise. The returned report's `file_errors` are now the
  union of all three passes' rather than the last one's — a tally is
  whole-store, but a file error belongs to the pass that tried to read the
  file. None of the four gated corpora is mixed, so no committed number moves;
  the fixture is the gate.

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
