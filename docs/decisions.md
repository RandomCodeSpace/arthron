# Decision log

Newest first. Each entry records what was decided, why, and what was rejected.

---

## 2026-07-27 — Measurement write-ups are local; decisions carry the numbers

**Decision:** `docs/evidence/` is untracked and local, alongside
`docs/superpowers/`. `docs/decisions.md` is the only public record. Numbers
that justify a decision are quoted inline here; the raw write-ups stay off the
public repository, and so does anything naming a private repository's
internals.

**Why:** this repository is public, and the baseline write-up cited the
predecessor's internal file paths and line numbers to attribute root causes.
That is the right level of rigour for an internal document and the wrong thing
to publish — more so now that the predecessor repository is being deleted, at
which point those paths reference nothing anyone can check.

**What was preserved:** the measurements themselves. Every number the write-ups
supported — the 1-call-edge graph, the redb stress-test figures, the 46.8% Go
baseline — is quoted in the entries above and below. Nothing that justified a
decision was lost; only the private repository's structure was dropped.

**Consequence for the gate.** The entry
"[Gate baselines ratchet only by commit](#2026-07-26--gate-baselines-ratchet-only-by-commit)"
requires per-language baselines to be committed. The narrative write-up no
longer is. When `arthron gate` is built it needs a small tracked
machine-readable baseline file — the rate per language and nothing else, which
carries no private detail. Recorded here so the two decisions do not silently
contradict each other.

**Measured Go baseline, for that file when it exists:** `go = 0.468`
(resolved 4,467; external 6,083; unresolved 5,077 — `NeedsTypeInference` 4,826,
`NoMatchingDefinition` 251), on the `codeiq` corpus at `6dd90b5`. Predecessor
baseline on the same corpus: 0%.

---

## 2026-07-26 — First milestone: walking skeleton, Go first

**Decision:** the first milestone is a thin vertical slice through all five
layers — ast-grep → extractor → resolver → store → `arthron scan` printing a
per-language resolution rate. Definition of done: **a non-zero per-language
resolution rate from a real repository, with every unresolved reference
persisted and countable by reason.** Not high — non-zero and honest.

**Language order:** Go, then Java, JavaScript, TypeScript, Python. Go because
its resolution model is the cleanest — explicit import paths, package scoping,
no overloads, no path aliasing — the only tier-1 language where a human can
eyeball a package and predict the rate, so a bad number is attributable to the
pipeline rather than the rules. JS and TS are expected to share one
module-resolution core.

**Corpus:** a vendored, pinned snapshot of `codeiq@6dd90b5` — 808 files,
105k LOC, 3.2 MB of source. The exact code that measured 0%, so the
before/after is direct. Vendoring resolves redistributability for Go; corpora
for the other four tier-1 languages remain open.

**Rejected:** extraction-breadth-first (rebuilds `codeiq`'s shape — broad
extraction, no proof of linking) and resolver-first against hand-written
fixtures (fixtures you author are fixtures you author to pass).

---

## 2026-07-26 — Extraction: in-process ast-grep crates; coverage corrected to 27

**Decision:** link `ast-grep-core`, `ast-grep-language` and `ast-grep-config`
in-process, behind a thin internal wrapper module. The wrapper exists because
the Rust API is 0.x and not a stability-guaranteed surface — the blast radius
of an ast-grep upgrade must be one file.

**Correction:** the "32 languages" claim counted the CLI's language registry,
which includes dynamically loaded grammars. `ast-grep-language`'s
`builtin-parser` feature ships **27** grammars. Coverage is 27; tier 2 is the
remaining 22. README and design doc corrected.

**Rejected:** shelling out to the `ast-grep` CLI (breaks the single-binary
promise, adds version drift and JSON ser/de on the hot path); raw `tree-sitter`
(loses the YAML rule layer, hand-maintaining queries for 27 grammars);
`ast-grep-dynamic` dylibs to keep the 32 figure (re-breaks single-binary).

---

## 2026-07-26 — Vocabulary: extractor, not detector

**Decision:** the single-file layer is the **extractor**. `detector` is retired
outside historical discussion of `codeiq` — a detector finds things and
decides, which is exactly the authority this layer does not have. Canonical
terms live in [`CONTEXT.md`](../CONTEXT.md).

---

## 2026-07-26 — Graph model: a node is a thing a reference can name

**Decision:** nodes are definitions, modules/packages, and external packages —
nothing else. Files are fields on definitions, not nodes. Locals never enter
the graph (nothing outside their scope can name them). `contains`/`defines`
edges do not exist. **An edge means exactly one thing: a reference resolved.**

**Why:** ~16k of `codeiq`'s ~28k edges were containment bookkeeping
(`contains` 13,232 + `defines` 2,991) — structural facts a struct field states
for free, and the reason detectors had to hand-emit "anchor nodes" to satisfy
the phantom-drop filter. With edge = resolution, edge count directly measures
whether the tool works. `impact <path>` improves: look up the file's
definitions, walk inbound resolved references, answer by symbol.

**Rejected:** heterogeneous node/edge kinds (`codeiq`'s shape) and split
file-graph + symbol-graph stores (two invalidation paths).

---

## 2026-07-26 — Identity: content-addressed 128-bit NodeId

**Decision:** `NodeId = hash(language, canonical fully-qualified name)`,
128-bit. Canonical-FQN construction is per-language resolver code (Go:
`module/pkg.Ident`; Java will need signature or arity for overloads). Hash
function choice deferred to dependency vetting.

**Why:** one B-tree probe per resolution (miss = `Unresolved`, recorded);
deterministic across machines and runs, so graphs are diffable and the CI
cache artifact is portable; extraction parallelises with no ID coordination;
edges become fixed-size PODs — which is what zerocopy was selected for.

**Rejected:** store-assigned counter (second lookup, serialised inserts,
machine-bound graphs); span-in-hash — a one-line edit at the top of a file
would change every ID below it and cascade a whole-repo re-resolve, a
plausible mechanism for `codeiq`'s 21.78s cold / 21.91s warm.

---

## 2026-07-26 — Symbol table lives in the store; cold is a special case of warm

**Decision:** phase 1 writes definitions to redb; phase 2 resolves by probing
redb. No in-memory symbol table, no separate incremental mode. **A cold index
is a warm index whose changed set is every file** — one code path that cannot
silently skip work, which is the failure `codeiq`'s separate incremental path
had.

**Arithmetic (not a new measurement):** at 5M LOC, 300k–1M references × one
probe each against the measured 854,782 reads/s on 2 vCPU ≈ 0.4–1.2s of probe
time inside a 30s cold budget.

**Rejected:** in-memory map (unbounded structure under a hard 512 MB ceiling,
plus a second implementation for incremental); hybrid (the least-exercised
path in development becomes the most-run path in production).

---

## 2026-07-26 — Unresolved references: one row per (file, kind, raw_target)

**Decision:** an unresolved reference is stored deduplicated per
`(file, kind, raw_target)` with reason, occurrence count and first span.

**Why:** a file is re-extractable — spans are derived data regenerable in
microseconds by re-parsing one file; the resolution outcome required the
whole-repo symbol table and is the expensive fact. Counts stay exact (the
gate's denominator is never sampled), per-file queries stay direct, and
distinct-target diagnostics ("`fmt.Println` unresolved in 800 files") survive.
A generated file with 10,000 identical calls is one row, bounding the
duplication blowup (design §3.3) structurally.

**This narrows §2.2's original "the reference itself" wording** — amended in
the design doc. Nothing is silently discarded and no count is approximated.

**Rejected:** one row per site (~140 MB of reconstructible spans at 5M LOC);
counts-only (says the rate is bad and nothing about why — where `codeiq` left
off).

---

## 2026-07-26 — Invalidation: candidate-set inverted index

**Decision:** resolution computes an ordered candidate-FQN list per reference;
every reference is indexed under **every candidate hash it probed — misses
included**. When a definition with hash H is added or removed, re-resolve
exactly the references indexed under H. Additions, removals and shadowing are
one mechanism: a reference that resolved via candidate #3 is also indexed
under #1–2, so a later higher-priority definition correctly re-points it.

Full re-resolve is retained as the **test oracle**: a mode that re-resolves
everything and diffs against the incremental result — the check `codeiq` never
had.

**Why:** the same insight as `Unresolved`-as-data — a failed probe is
information; recorded, it becomes the invalidation trigger. Cost is
O(candidates × references) index entries, with candidates a small per-language
constant (Go ~2–4).

**Rejected:** full re-resolve in production (~0.4–1.2s per event kills the
sub-millisecond watch inner loop); module-level coarse invalidation
(over-invalidates importers, under-invalidates unresolved references unless
the same bookkeeping returns through the back door).

---

## 2026-07-26 — Gate baselines ratchet only by commit

**Decision:** per-language baseline rates are committed to the repository.
`arthron gate` fails when a language drops below its baseline; the baseline
moves upward only by a deliberate commit, never automatically.

---

## 2026-07-26 — Daemon owns the single writer

**Decision:** the daemon holds redb's one write handle; watch-mode indexing
goes through it. CLI queries and the MCP server read MVCC snapshots. This is
the shape the stress test measured: 854,782 reads/s sustained against a
continuous writer, worst read 13.65ms.

---

## 2026-07-26 — Name reserved on crates.io and npm

**crates.io: published.** `arthron 0.0.0`, owner `aksOps`, MIT,
https://crates.io/crates/arthron

**npm: published to GitHub Packages, not npmjs.org.**
`@randomcodespace/arthron@0.0.0`, linked to this repository,
https://github.com/orgs/RandomCodeSpace/packages/npm/package/arthron

Scoped to the org rather than taking the bare `arthron` name on the public
registry. Two consequences worth knowing:

- **Package visibility is `private`.** GitHub Packages defaults to it and there
  is no REST endpoint to change it — it is a UI setting under the package's
  settings. Fine for holding the name; needs flipping before public
  distribution.
- **`arthron` on npmjs.org is still unclaimed by us.** If `npx arthron` (bare,
  unscoped) is ever wanted, that name is not reserved and someone else can take
  it. `npx @randomcodespace/arthron` from GitHub Packages requires consumers to
  configure a scope mapping and a token, which is friction npmjs.org would not
  have.

**Not an empty stub, deliberately.** crates.io policy forbids a crate that

> exists only to reserve a name for a prolonged period of time (often called
> "name squatting") without having any genuine functionality, purpose, or
> significant development activity on the corresponding repository

and the team may delete such crates *without prior notification*. So `0.0.0`
ships the resolution contract itself — `Outcome` with its three variants and
`resolution_rate` — which is the one type the whole design is built around, and
the README states plainly that it is not usable software and should not be
depended on. The public repository with a full design spec is the "genuine
purpose and development activity" the policy asks for.

**Note the irreversibility.** crates.io versions can be yanked but never
deleted, and the name is held permanently. `arthron` on crates.io is now
committed to.

---

## 2026-07-26 — Name: `arthron`

**ἄρθρον** · AR-thron · *joint*.

In Greek anatomy, the articulation where two separate bones meet and move as one
— the root of *arthro-*. In Greek grammar, the *article*: the small word whose
only job is binding a reference to its referent.

Both senses describe the resolver. Two files are parsed in isolation, knowing
nothing of each other; the joint is what makes them one graph. And a joint
either articulates or it does not — which is the `Resolved` / `Unresolved`
contract.

**Availability, checked 2026-07-26:**

| Registry | Status |
|---|---|
| crates.io | free |
| npm | free |
| GitHub exact-name | 2 repos, both empty, 0 stars |

### Rejected candidates

Availability checked against crates.io, npm, and GitHub exact-name matches.

**Taken — including two in this exact product space:**

| Name | Status |
|---|---|
| `onoma` (ὄνομα, *name*) | **crates.io taken** — "language-agnostic semantic symbol indexer" |
| `hodos` (ὁδός, *path*) | **crates.io taken** — "policy-driven graph traversals" |
| `gnomon` | crates.io taken — "performance budget auditor, a CI gate not a dashboard" |
| `mitos`, `horos`, `nema`, `plegma`, `kanon`, `tekton` | crates.io taken |
| `skopos`, `dromos`, `poros`, `ichnos` | npm taken |

**Free but rejected on merit:**

| Name | Meaning | Why not |
|---|---|---|
| `desis` (δέσις) | the *tying* — Aristotle's *Poetics* | **Meaning runs backwards.** In the *Poetics*, `desis` is the knot and `lusis` the untying. This tool is the untying. Also: 20+ exact-name GitHub repos, all zero-star Spanish "prueba técnica de Desis" — DESIS is a Chilean consultancy whose take-home test candidates all push as `desis`. Plus DESIS Network (design academia) and Desis (Bayer insecticide). Reads as a typo of *thesis*. |
| `lusis` (λύσις) | *untying, resolution* | Genuinely strong — exact meaning, 5 letters, free everywhere, 2 junk repos. Echoes *lysis* (cell rupture); `github.com/lusis` is a longstanding engineer's handle. Lost to `arthron` on the owner's call. |
| `harmos` (ἁρμός) | the fitted masonry *joint* | Same meaning as `arthron`, but the string leads with `harm` — "harmos failed" reads wrong. |
| `tekmerion` (τεκμήριον) | *conclusive proof* — Aristotle's necessary sign, opposed to `semeion`, the fallible one | Best meaning-fit of any candidate: it names the Resolved/Unresolved distinction exactly. Nine letters, too long. |
| `horismos` (ὁρισμός) | *definition*, from `horos`, the boundary stone | Emptiest namespace found (1 repo). Awkward to say. |
| `syndesmos` (σύνδεσμος) | *bond*; in grammar, the conjunction | Nine letters. |
| `symploke` (συμπλοκή) | *interweaving*; in Stoic logic, connection of propositions | Pronunciation ambiguous. |
| `katalepsis` (κατάληψις) | Stoic: an impression grasped so firmly it cannot be false | Collides with a popular web serial. |
| `zeugma` (ζεῦγμα) | *a yoking*; one word governing many | ZOOG-ma or ZYOOG-ma — not obvious on sight. |
| `anaphora` | linguistics: a reference pointing back to its antecedent | Free on both registries, but already a programming term (anaphoric macros in Lisp) and 134 GitHub repos. |

**Non-Greek round, all rejected:** `heddle`, `plumbline`, `catena`, `throughline`,
`warpline`, `codeweft`, `holdfast`, `sinew`, `cartogram`. Owner asked for Greek.

**Criteria that decided it:** small, easy to pronounce, meaning that matches what
the resolver actually does, and a clean namespace on crates.io, npm and GitHub.

---

## 2026-07-26 — Store: redb + bincode + zerocopy

**Decision:** redb 4.1.0 for the embedded store, bincode for node and file
records, zerocopy for fixed-size edge PODs.

**Constraint given:** performant, on-disk, actively maintained. Cross-process or
cross-language access explicitly not required if it costs performance.

**Stress-tested before committing**, modelling the 5M-LOC target at 152k nodes
and 114k edges on 2 vCPU. The single-writer concern did not survive
measurement: under continuous write pressure, readers sustained 854,782
reads/s with a 13.65ms worst case. Baseline build 592.69ms; single-file save
494.67µs average, 1.04ms worst; 500 files in one transaction 59.97ms against
216.04ms as 500 transactions (3.6×, real but not a wall); `db.compact()`
returned a churned 257 MB file to 125 MB in 1.39s.

**Rejected:** `sled` (still `1.0.0-alpha.124`, last touched 2024-10-11) and
`rkyv` (actively maintained, but recent work is fuzzer fixes for UAF and type
confusion in the zero-copy access path — wrong risk profile for reading a
possibly-corrupt CI-restored cache).

---

## 2026-07-26 — Architecture: detectors emit references, not edges

**Decision:** detectors are forbidden from emitting edges. They emit
`Reference { kind, raw_target, scope, span }`. A single resolver owns all
linking and classifies every reference as `Resolved`, `External`, or
`Unresolved`. **It never drops.**

**Why:** the predecessor let 100+ detectors build edges, then silently
discarded any edge whose endpoints were not already known. Measured on a
1.33M-line corpus: 14,423 method nodes produced **1** call edge; edge kinds
were `contains` 13,232, `imports` 11,843, `defines` 2,991, `calls` 1;
confidence was `LEXICAL` 24,454, `SYNTACTIC` 5,831, `RESOLVED` **0**; and
4,190 external nodes were created and referenced by nothing. 102 of 107
detector files attempted no cross-file work at all.

A detector sees one file. It cannot know whether a target exists elsewhere. So
it either guesses (dropped) or gives up (nothing). Only a layer that sees all
files can link them.

---

## 2026-07-26 — Language capability tiers

**Decision:** coverage stays at ast-grep's full 32 languages. Capability tiers:

- **Tier 1** (definitions + references + call graph): Java, TypeScript, Python, Go, JavaScript
- **Tier 2** (definitions + structure): the remaining 27

**Why:** the owner asked not to reduce coverage, and to treat framework and
language support as day-one requirements. Tiering satisfies both without
pretending to resolution the tool cannot prove. Tier 2 reports what it can
verify and marks the rest `Unresolved` rather than inventing edges.

---

## 2026-07-26 — Primary gate is resolution rate, not performance

**Decision:** per-tier-1-language resolution rate is the top-ranked gate. A
regression fails the build. Reported per language, never aggregated.

**Why:** `codeiq` was fast and returned nothing useful. Optimising a tool that
resolves 0% of references is optimising the wrong number. Performance budgets
(§3.2 of the design) are secondary — with resource ceilings hard and timing a
target.

Baseline today: **0% for every language.**

---

## 2026-07-26 — Rust, greenfield

**Decision:** rewrite as one Rust binary rather than merging the three Go/Java
repos.

**Why not Go:** `codeiq` saturates at ~4 of 8 cores from lock contention, and
Rust does not fix that by itself — a mutex is a mutex. The decisive reason is
the edge model, not the language. But given a rewrite is required either way,
the owner's requirement for hard resource ceilings on a 2 vCPU CI runner favours
Rust.

**Why not fork existing work:** `colbymchenry/codegraph` (62,540★, MIT, Rust)
has no plugin mechanism and does no quality analysis. `Jakedismo/codegraph-rust`
(850★) has no license at all.

**Merged from:** `codeiq`, `code-signal`, `sonar-predict` — the "Code-signal"
cluster grouped during the 2026-07 portfolio audit.

---

## 2026-07-26 — No frontend

**Decision:** CLI, MCP, daemon and CI gate only. No graph visualisation UI.

**Why:** owner cut it explicitly. The two driving use cases — incremental
re-index during agent-assisted development, and full-graph MR review in CI — are
both non-interactive.
