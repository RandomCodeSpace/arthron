# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Pre-1.0, the public API and the CLI surface may change in any release.

Decisions and their rationale — including what was rejected — live in
[`docs/decisions.md`](docs/decisions.md); this file records what shipped.

## [Unreleased]

### Added

- **`arthron pin`, and a committed target pin per tier-1 corpus: the gate can
  now see a wrong edge.** `arthron gate` compares four integers — `resolved`,
  `external`, `local_binding`, `unresolved` — and a reference that resolves to
  the *wrong* definition moves none of them. It is still one `Resolved` row and
  still one edge; only the far end changed. The rate cannot see it, the
  `denominator_shrank` check cannot see it, and neither drift check can, because
  not one of them reads a target. That blind spot is the whole of the standing
  verdict that *a wrong edge is worse than a miss, because a miss is counted and
  a wrong edge is not*.

  `pins/<corpus>.pins` records the target of every resolved reference row, per
  corpus, for the fifteen tier-1 corpora — 80,272 rows over 18,687 distinct
  targets. `tests/edge_pins.rs` scans each corpus cold and compares. The rule:

  - a pinned row whose target **changed** fails, by name — `target_moved` —
    printing the file, the line, the reference kind, the enclosing FQN, the
    site text, the arity, the old target and the new one;
  - a row that **appeared** is coverage growth and passes;
  - a row that **vanished** is flagged in the output and does not fail: the
    counting gate owns that half, and a re-pin that drops rows shows the drop
    as deleted lines in the pin file's own diff.

  **The format, and why.** Written out in full those rows are 14.5 MB of
  committed text (measured). Stored as a 64-bit hash of the store's own
  canonical row key plus an index into a plaintext, deduplicated dictionary of
  target names, the fifteen files are 3.1 MB. The target names stay in
  plaintext deliberately: a check whose failure printed
  `0x8f3a… became 0x1c07…` would tell a reviewer that something moved and
  nothing about what, so the old target is recoverable by name from the file
  and the new one — with the file, line and site text — is re-derived by
  joining the failing hash against the scan already in hand. Rows are grouped
  under their file so a re-pin's diff says which files' edges moved.

  **Regeneration is one command per corpus, and every pin file carries it in
  its own header:**

  ```
  arthron pin corpus/go/codeiq --pins pins/go-codeiq.pins --write --commit <sha>
  ```

  Verified by mutation, not by assertion. Making the Java member walk prefer a
  superclass's declaration over the receiver's own override left
  `java-commons-lang` at `resolved 34217 / unresolved 16279 / external 63385 /
  local_binding 15162` and `java-gson` at `12885 / 6105 / 16737 / 6706` —
  byte-identical to their baselines, both gates green — while the pin check
  failed both, naming 275 and 266 moved edges with 0 appeared and 0 vanished
  (`ImmutableTriple.of` resolving to `Triple.of`, `new JsonTreeReader`
  resolving to `JsonReader.<init>`). Reverting made all 21 pin tests green
  again.

  No workflow change: `.github/workflows/gate.yml` already ends by running the
  whole suite with `ARTHRON_REQUIRE_CORPUS=1` in the one job that fetches the
  corpus, so the fifteen comparisons run and block a merge there. Fifteen cold
  scans, 7.4 s wall.

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

- **A probe corpus for each of the other four tier-1 languages, and the four
  gates that pin them.** Go had a synthetic corpus that states a resolver
  outcome per named site; Java, JavaScript, TypeScript and Python did not, so
  every method-call outcome in four of the five languages was observable only
  as its contribution to a real corpus's totals — where a fix and a regression
  of the same size cancel. `corpus/java/probes`, `corpus/javascript/probes`,
  `corpus/typescript/probes` and `corpus/python/probes` are hand-written truth
  tables: every call site is asserted by name, hit or miss, with the reason a
  miss carries. Baselines, `tests/baselines.rs` `GATED` rows and steps in
  `.github/workflows/gate.yml` land with them — twenty-five gates become
  twenty-nine.

  | baseline | resolved | external | local_binding | unresolved | rate |
  |---|---:|---:|---:|---:|---:|
  | `java-probes` | 13 | 7 | 1 | 1 | 92.9% |
  | `javascript-probes` | 6 | 0 | 1 | 2 | 75.0% |
  | `typescript-probes` | 12 | 0 | 1 | 3 | 80.0% |
  | `python-probes` | 5 | 0 | 2 | 1 | 83.3% |

  These four and `go-probes` are **pins, not ratchets**. The corpora are
  hand-written, so their rates are properties of the fixtures and are not
  evidence of a capability; re-basing one to claim a better number would be
  claiming something nobody measured. They are in the README's tier-1 table
  because they are gated exactly like the others, marked `†` so no reader takes
  them for a sample of real code.

  **The misses are pinned as exactly as the hits, including two that are filed
  under the wrong reason.** `super.greet` and `this.greet` are the same call one
  keyword apart and get different answers: `super.` walks the written `extends`
  into the other module, `this.` does not. The failure is then reported as
  `UnindexedSupertype`, whose definition in `src/lib.rs` requires the receiver
  type to be in-repository, the member to be in no indexed supertype, and some
  supertype to be external or unindexed — and on this row **all three conjuncts
  are false**, provably, because the same scan resolves `super.greet` through
  that supertype. One missing branch causes it: `walk_members`
  (`src/track_ecma/resolve.rs`) probes the base under a module-local id, so an
  imported base always misses, and only `resolve_super` carries the
  import-following fallback. Both ECMAScript dialects assert it, because the
  defect belongs to the shared track rather than to either language. A probe is
  a truth table, so this is recorded as what *is* rather than what should be —
  recording the mislabel is what stops it surfacing later as an unattributable
  movement. Its fix is measured, not guessed: giving `resolve_this` the
  fallback moves `javascript-probes` to 87.5% (resolved 6 → 7) and
  `typescript-probes` to 86.7% (resolved 12 → 13) and moves **nothing** on
  express, fastify, vue-core or zod, so it is a deliberate re-base of two pins
  plus a `docs/decisions.md` entry, and no ratchet is touched.

  TypeScript's probe adds a third row that keeps a *different* reason on
  purpose: `this.inner.greet` is `NoMatchingDefinition`, the bucket that in a
  corpus which compiles means arthron's own bug — and this corpus compiles
  (`tsc --noEmit`, `strict`, clean). Three annotations naming a class two lines
  of import away land in three different buckets while every one of them
  resolves as a `TypeUse`: the type is read and simply not used to type a
  receiver.

- **The Python census walks named trees instead of a whole language
  directory.** `tests/python_corpus.rs` walked `corpus/python` entire — the one
  whole-language walk in `tests/`, where every other corpus test names its tree
  — which made its constants a function of the *corpus repository's* contents
  rather than of this repository's extractor. `gate.yml` checks the corpus out
  at `ref: main`, unpinned, so any commit adding a `.py` file anywhere under
  `corpus/python` would have turned the test red with nothing here changed;
  adding `corpus/python/probes` would have done it immediately. It now names
  `corpus/python/django` and `corpus/python/flask`, restoring the property that
  a census moves only when the extractor does. Paths stay relative to
  `corpus/python`, so every module name derived from one is byte-identical and
  no constant moves. `corpus/python/probes` is deliberately outside it: a probe
  is pinned row by row in `tests/python_probes.rs`, which is a stronger check
  than a total that would blur it.

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
  `rate denominator 14089 of 36557 references (38.5%)`: `(resolved +
  unresolved)` over every reference the language emitted. Excluding `external`
  and `local_binding` from both of the rate's terms is correct and it also
  makes the denominator a fraction of the surface — codeiq's Go rate of 69.5%
  covers 38.5% of Go's references, fastify's 63.0% covers 14.2% of
  JavaScript's — and a rate published without its share reads as a claim about
  the whole. The codeiq figures quoted here are the ones this release ships,
  not the ones the feature was written against: the Go field-access entry above
  moved them inside this same unreleased section. Text report only, on `scan` and on `gate`. **`--json` is
  unchanged and its `schema` does not move**: the document already carries all
  four counts, so a consumer derives the share exactly, and a field for
  arithmetic is not a field.

- **The tier-1 claim is retracted to what is measured.** The README, the
  changelog and the report line called tier 1 "call-graph resolution". It is
  not: method dispatch mostly does not resolve, and the buckets that need a
  type environment — `NeedsExpressionType`, `NeedsReceiverType`,
  `NeedsTypeInference`, `AmbiguousOverload`, `UnindexedSupertype`,
  `DynamicDispatch` — are the majority of what tier 1 leaves unlinked on nine
  of the ten real corpora, 51.9% on flask to 100.0% on both Go corpora, the
  tenth being vue-core at 42.1%. This entry first said the single reason
  `NeedsTypeInference` was "most of what tier 1 leaves unlinked in all five
  languages", citing 758 of codeiq's 884 unresolved rows. That was an
  over-attribution and is corrected here: it holds for no language once the
  reasons are counted — commons-lang is led by `AmbiguousOverload` (9,218 of
  16,279) and gson by `NeedsExpressionType` (4,713 of 6,105), where
  `NeedsTypeInference` is 342 and 72 rows — and codeiq's own leading bucket is
  now `NeedsReceiverType`, 3,116 of 4,295. What the leading buckets share is
  the type environment, which is the work; no one of them stands for it. A call
  through a receiver whose type its own signature states does resolve, in all
  five, since the locals re-base above. Tier 1 now
  reads "definitions, references, and cross-file import and function-call
  resolution", and the scan line prints `tier 1: call, import and type-use
  resolution` — which is what the denominator holds. Nothing measured changed;
  no baseline moved.

### Fixed

- `arthron pin` compared the tree a pin file names against the tree it scanned
  with the platform's own path semantics, while the header can only ever hold a
  `/`-separated path. On Windows the two forms differ, so whether a pin file
  matched its own tree was decided by the separator rather than by the tree,
  and the refusal printed one path each way — reading as if the separator were
  the difference. Both sides are now normalised before the comparison and in
  the message. Found by the Windows CI job, which had never reached the
  positive half of the check.

- **Every number the README published was stale, and nothing could have caught
  it.** Three re-bases moved the tier-1 counts — the locals unification, Go's
  field-access surface, and the ECMAScript config and globals work — and four
  probe baselines landed, while the tables, the `arthron scan` sample and the
  prose around them still stated the pre-wave figures. A gate compares a scan
  against a baseline and has no opinion about prose, so all twenty-nine gates
  were green over a README that was wrong in every tier-1 row. Both tables are
  now re-rendered from `baselines/*.toml`: fifteen tier-1 rows including the
  five probe pins, fourteen tier-2 rows unchanged, and both derived columns
  recomputed rather than carried forward.

  `every_readme_table_row_matches_its_baseline` and
  `every_published_rate_carries_its_denominator_share` in `tests/baselines.rs`
  are what make the next drift fail instead of ship. The first re-derives all
  eight cells of every row — the four gated counts, the commit pin, the rate
  and the denominator share — and asserts one row per committed baseline, so a
  baseline with no row and a row with no baseline both fail. The second checks
  the commitment the README makes in prose, that no rate is published without
  its share, as a shape rather than a sentence. Both read `README.md` and
  `baselines/` only, so they run in CI where the corpus is absent and every
  ratchet skips.

- **The retraction over-attributed the gap to one reason.** The README, this
  changelog and `Lang::tier`'s doc comment all said `NeedsTypeInference` was
  most of what tier 1 leaves unresolved, citing 758 of codeiq's 884 unresolved
  rows. Counted per reason on all ten real corpora it holds for none of them —
  commons-lang is led by `AmbiguousOverload` (9,218 of 16,279), gson by
  `NeedsExpressionType` (4,713 of 6,105) where `NeedsTypeInference` is 72, and
  codeiq's own leading bucket is now `NeedsReceiverType` at 3,116 of 4,295.
  What is true, and is what the three now say, is that the reasons *needing a
  type environment* are together the majority on nine of the ten, 51.9% on
  flask to 100.0% on both Go corpora, with vue-core the tenth at 42.1% because
  the injected test-runner globals outnumber them there. Replacing one reason
  with the family it belongs to is the same retraction the tier-1 claim already
  made, one level down.

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
