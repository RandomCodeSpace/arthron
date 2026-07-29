# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Pre-1.0, the public API and the CLI surface may change in any release.

Decisions and their rationale — including what was rejected — live in
[`docs/decisions.md`](docs/decisions.md); this file records what shipped.

## [Unreleased]

### Changed

- **Go reads a member as well as calling one — the last two un-emitted Go
  reference sites, and a re-base of two baselines.** The extractor emitted a
  reference for a call through a selector and for a written type name, and
  nothing for the two other places the Go grammar names a member: a selector
  *read* (`pkg.Name`, `t.field`, `T.Method`, `x.y`) and a struct literal's
  field keys (the `Field` in `T{Field: v}`). Both are now
  `RefKind::FieldAccess` rows — 8,200 selector-read occurrences on `codeiq`
  and 9,947 on `caddy`, 3,134 and 3,776 literal keys — which is every one of
  those sites in both corpora, counted at the grammar and matched exactly by
  what the store holds.

  A read resolves the way a call of the same shape already did: `pkg.Name`
  through the import table, a receiver root as `this` (so `c.Name` reaches
  `Conn.Name`), a root some enclosing block binds as `LocalBinding`. What is
  new is the owner written *at the site* — `T.Method`, `T{Field: v}`,
  `pkg.T{Field: v}` — where the member is probed under that owner and a miss is
  answered by what the owner is: `NeedsReceiverType` when the owner is a type
  in this repository, `NeedsTypeInference` when it is not. Go struct fields are
  not nodes in this build, so an honest field read lands in the first of those,
  exactly as a receiver-rooted call already did. A map or array literal's key
  is an expression rather than a member name and is not a reference; an
  anonymous `struct{…}` has no canonical name, so neither it nor its fields are
  nodes and its keys name nothing.

  Both of those read the type *as written*, which is the whole of what one
  file says. `map[K]V{k: v}` is rejected because the site writes `map`;
  `type Registry map[K]V` used as `Registry{k: v}` is not, because the site
  writes a name and the declaration is usually in a sibling file. So a named
  map, slice or array type keyed by an identifier is reported as a member of
  it — 9 rows / 90 occurrences on `codeiq` (`CapabilityMatrix` in
  `internal/intelligence/query`), 0 on `caddy` — and lands in
  `NeedsReceiverType`, inside the denominator, understating the rate rather
  than flattering it: 69.5% as measured against 70.0% without those rows.
  Closing it needs a fact no single file holds, so it is stated where it is
  made (`named_type_path` in `extract_go.rs`, and `ref-litkey` in
  `rules/go.yml`) rather than fixed by guessing. What is closed is the harm:
  a literal key's member is never probed, so no such site can *link*. A named
  non-struct type may carry a method, and a method name and a map-key
  constant do not collide the way a method name and a field name do, so the
  probe could only ever have found the wrong node — and a wrong edge is
  strictly worse than an unresolved reference. Nothing a compiling corpus had
  earned is lost by skipping it: a Go struct field is not a node in this
  build, so every literal key on both corpora was already unresolved, and all
  three Go gates hold their counts to the row.

  Separately, **`NoMatchingDefinition` is now empty on both Go corpora**, from
  123 rows on `codeiq` and 269 on `caddy`. Every one of them was a predeclared
  type name at a conversion — `string(b)`, `int64(n)`, `byte(c)` — which Go
  writes exactly as it writes a call, so the grammar filed it as a
  `call_expression` and the resolver checked it against the predeclared
  *functions*. The name was never absent; it was in the universe block, one
  list over. That bucket's contract is that the lookup table was complete and
  the name missing, which in a corpus that compiles means arthron's own bug, so
  a row that does not mean that does not belong in it. A type cannot be called,
  so a one-argument call naming one is a conversion and nothing else.

  Measured, release build, cold store:

  | baseline | resolved | unresolved | external | local_binding | rate |
  |---|---:|---:|---:|---:|---:|
  | `go-codeiq` | 8,016 → 9,794 | 884 → 4,295 | 12,210 → 12,595 | 4,113 → 9,873 | 90.1% → **69.5%** |
  | `go-caddy` | 10,208 → 10,585 | 2,700 → 9,014 | 19,201 → 21,304 | 8,252 → 13,181 | 79.1% → **54.0%** |
  | `go-probes` | 17 | 0 | 26 | 1 | 100.0% |

  `go-probes` is byte-identical: it writes no selector read and no keyed
  literal. No other baseline is touched — this is a Go rule file and a Go
  resolver.

  **A rate that falls here is not a regression.** It is the same argument the
  `LocalBinding` unification made in the other direction: what is *in* the
  rate's terms changed, so the two numbers are not measurements of the same
  thing. Go's denominator — `resolved + unresolved` — grew from 8,900 to 14,089
  on `codeiq` and from 12,908 to 19,599 on `caddy`, which is 5,312 and 6,960
  new references inside the rate's terms less the 123 and 269 the conversion
  fix moved out of it. 1,778 and 377 of the new occurrences resolved to a
  definition, and nothing that resolved before stopped resolving.

  **Attributed per row, not inferred from the totals.** A whole-row join
  between a binary built from the previous commit and this one, keyed
  `file + kind + declaration space + enclosing FQN + site text + argument count
  + locally-bound`, over both corpora:

  - The conversion fix moves **89 pre-existing rows on `codeiq` (123
    occurrences) and 199 on `caddy` (269)**, every one of them
    `NoMatchingDefinition → External("go:builtin")`, and nothing else: no row
    added, none removed, and `resolved`, `local_binding` and every other reason
    identical on both sides.
  - The two new constructs then change **zero** pre-existing rows. Every
    movement is a new row, all of kind `field-access`: 7,436 rows / 11,334
    occurrences on `codeiq` and 8,332 / 13,723 on `caddy`. Split by construct
    and outcome —

    | corpus | construct | resolved | external | local_binding | NeedsReceiverType | NeedsTypeInference | NeedsExpressionType |
    |---|---|---:|---:|---:|---:|---:|---:|
    | `codeiq` | selector reads | 1,778 | 51 | 5,745 | 205 | 12 | 409 |
    | `codeiq` | literal keys | 0 | 211 | 15 | 2,908 | 0 | 0 |
    | `caddy` | selector reads | 377 | 1,184 | 4,634 | 2,904 | 541 | 307 |
    | `caddy` | literal keys | 0 | 650 | 287 | 2,825 | 6 | 0 |

    — where each row of the table sums to that construct's whole site count as
    counted at the grammar: 8,200 and 3,134 on `codeiq`, 9,947 and 3,776 on
    `caddy`. `caddy`'s literal keys sum to 3,768 there and not 3,776 because
    the last eight land on two rows the *first* construct created rather than
    on rows of their own: `TestBuffering` declares a function-local
    `type args`, and the read `args.body` and the four literal keys
    `args{body: …}` are one target, `LocalBinding` either way, so each of the
    two keys they share carries five occurrences instead of one.

  The reference census in `tests/corpus.rs` is new and is what makes this
  observable next time: a rule that stops being emitted moves no baseline —
  the gate compares four occurrence totals and another rule can supply them —
  and moves no reason bucket either. Rows *and* occurrences are now pinned per
  kind on both corpora, because a rule that stops deduplicating moves one and
  not the other.

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
