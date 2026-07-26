# arthron — working notes

Read these before doing anything in this repo:

1. [`docs/superpowers/specs/2026-07-26-arthron-design.md`](docs/superpowers/specs/2026-07-26-arthron-design.md)
   — the approved design. Start here.
2. [`docs/evidence/2026-07-26-baseline-measurements.md`](docs/evidence/2026-07-26-baseline-measurements.md)
   — the measurements every design claim rests on.
3. [`docs/decisions.md`](docs/decisions.md) — decision log, newest first.
4. [`CONTEXT.md`](CONTEXT.md) — the project glossary. Use its terms; respect
   its `_Avoid_` lists.

**Status:** design approved, no implementation yet. The next step is an
implementation plan (`superpowers:writing-plans`), not code.

## Non-negotiables

These came out of measuring what the predecessor got wrong. Changing one means
updating the design doc and saying why.

- **Extractors never emit edges.** They emit `Reference { kind, raw_target, scope, span }`.
  One resolver owns all linking.
- **The resolver never drops.** Every reference is `Resolved`, `External`, or
  `Unresolved` — and `Unresolved` is stored with a reason, not discarded.
- **Resolution rate is the primary gate**, ranked above performance, reported
  per tier-1 language and never aggregated. A regression fails the build.
- **2 vCPU is the reference hardware**, not the developer's box. Resource
  ceilings are hard (< 512 MB RSS); timing is a target.
- **No network calls.** Ever. Everything local.

## Conventions

Rust. Record new decisions in `docs/decisions.md` — newest first, with what was
rejected and why. New measurements go in `docs/evidence/` and must be executed,
never estimated; if a benchmark turns out to be invalid, keep it with the reason.
