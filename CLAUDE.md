# arthron — working notes

Read these before doing anything in this repo:

1. `docs/superpowers/specs/2026-07-26-arthron-design.md` — the approved design
   (local, untracked). Start here if you have it; otherwise `docs/decisions.md`
   carries every decision and its rationale.
2. [`docs/evidence/2026-07-26-baseline-measurements.md`](docs/evidence/2026-07-26-baseline-measurements.md)
   — the measurements every design claim rests on.
3. [`docs/decisions.md`](docs/decisions.md) — decision log, newest first.
4. [`CONTEXT.md`](CONTEXT.md) — the project glossary. Use its terms; respect
   its `_Avoid_` lists.

**Status:** walking skeleton implemented — `arthron scan` prints a real Go
resolution rate against `corpus/go/codeiq`. Next: gate command and baseline
ratchet, then Java.

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

Design and discussion sessions happen in the **main checkout**; their artifacts
(specs, plans) live in `docs/superpowers/` — local, untracked, never committed.
Implementation happens in **worktrees**. The public record is `docs/decisions.md`
and `docs/evidence/` only.

Rust. Record new decisions in `docs/decisions.md` — newest first, with what was
rejected and why. New measurements go in `docs/evidence/` and must be executed,
never estimated; if a benchmark turns out to be invalid, keep it with the reason.
