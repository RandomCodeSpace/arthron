# arthron

**ἄρθρον** — *joint*. In Greek anatomy, the articulation where two separate bones meet
and move as one. In Greek grammar, the *article* — the small word that binds a
reference to the thing it refers to.

A local-first code intelligence engine. It parses each file in isolation, then
**resolves** the references between them into a verified graph — and tells you
honestly which references it could not resolve.

> **Status: design phase.** No implementation yet. The approved design lives in
> [`docs/superpowers/specs/2026-07-26-arthron-design.md`](docs/superpowers/specs/2026-07-26-arthron-design.md).
> Start there.

---

## Why this exists

Its predecessor, `codeiq`, produced a graph containing **14,423 method nodes and
exactly one call edge**. Every edge it emitted was `LEXICAL` or `SYNTACTIC`
confidence; zero were `RESOLVED`. It reported success the whole time.

That is not a bug in one function. It is what happens when 100+ independent
detectors each hand-build edges, and a central filter silently drops any edge
whose endpoints it does not recognise. The tool cannot tell you the blast radius
of a change, because it never actually linked anything.

`arthron` inverts that. Detectors are forbidden from emitting edges. They emit
**references** — "this call site names `parseConfig`, in this scope, at this
span." A single resolver owns linking, and every reference lands in exactly one
of three buckets:

| Outcome | Meaning |
|---|---|
| `Resolved` | Linked to a definition in this repo. Verified. |
| `External` | Linked to a known dependency outside the repo. |
| `Unresolved` | Could not be linked. **Recorded, never dropped.** |

**Resolution rate is the primary quality gate** — ranked above performance. A
change that lowers it fails the build. The honest baseline today is 0%.

See [`docs/evidence/2026-07-26-baseline-measurements.md`](docs/evidence/2026-07-26-baseline-measurements.md)
for the measurements behind every claim above.

## Shape

```
ast-grep ──▶ Extractor ──▶ Resolver ──▶ Store ──▶ Surfaces
 parse +      YAML rules:   refs →       redb      CLI · MCP ·
 match        defs, refs,   edges                  daemon · CI
              framework
              facts
```

Rust. No network calls. Everything local.

## Design targets

| Budget | Target |
|---|---|
| Cold index, 5M LOC | < 30s |
| Warm re-index, unchanged tree | < 3s |
| Peak RSS | < 512 MB |
| Reference hardware | **2 vCPU** — a CI runner, not a workstation |

The 2 vCPU constraint is deliberate. This tool runs in CI and in agent loops. It
must be predictable there, not just fast on a developer's laptop.

## Language coverage

All 32 languages ast-grep supports get structural analysis. Full call-graph
resolution ships for five to start:

- **Tier 1** (definitions, references, call graph): Java, TypeScript, Python, Go, JavaScript
- **Tier 2** (definitions and structure): the remaining 27

Tier 2 is a real product surface, not a stub — you get symbols, imports and file
structure. You do not get verified call edges, and the tool says so rather than
inventing them.

## License

MIT. See [LICENSE](LICENSE).
