# Decision log

Newest first. Each entry records what was decided, why, and what was rejected.

---

## 2026-07-26 — Name reserved on crates.io and npm

**crates.io: published.** `arthron 0.0.0`, owner `aksOps`, MIT,
https://crates.io/crates/arthron

**npm: staged, blocked on auth.** `npm/` is ready; publishing needs an
npmjs.org login this machine does not have.

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

**Stress-tested before committing** — full numbers in
[`evidence/2026-07-26-baseline-measurements.md`](evidence/2026-07-26-baseline-measurements.md) §5.
The single-writer concern did not survive measurement: under continuous write
pressure on 2 vCPU, readers sustained 854,782 reads/s with a 13.65ms worst case.

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

**Why:** `codeiq` let 100+ detectors build edges, then silently discarded any
edge whose endpoints were not already known. Result: 14,423 method nodes, 1 call
edge, 0 `RESOLVED` edges. See
[`evidence/2026-07-26-baseline-measurements.md`](evidence/2026-07-26-baseline-measurements.md) §2.

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
