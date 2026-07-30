# Decision log

Newest first. Each entry records what was decided, why, and what was rejected.

---

## 2026-07-30 — Stream C keeps array receivers in `NeedsTypeInference`

**Decided: a declared Java array receiver stops before ordinary canonical-type
placement and returns `NeedsTypeInference`.** The guard recognizes repeated
`[]` and varargs suffixes, so unqualified fields, static fields, and
`this.`-rooted fields all retain their existing C0 row keys while unsupported
array members remain honestly unresolved.

True local and parameter receivers remain `LocalBinding`: the extractor marks
them `locally_bound`, and the resolver's standing early return intentionally
precedes declared-type lookup. Reclassifying those rows would be non-AO movement
outside this repair; their full C0 keys are regression-tested unchanged.

*Rejected: resolve `length` or `clone()` as real or synthetic array members.*
That is an array-member modeling feature and would reclassify rows to
`Resolved`; it needs a separately attributable baseline change.

*Rejected: bypass the `locally_bound` short circuit for arrays.* That changes
legacy `LocalBinding` rows and violates Stream C's non-AO constraint.

---

## 2026-07-30 — the resolver owns reference-key refinement

C0 made file-local argument types part of reference-row identity before the
resolver owned the choice to use them. The failure was reproduced on a
pre-existing singleton Java call whose target and outcome stayed unchanged:
`rows_rekeyed=1`, with `1 appeared, 1 vanished, 0 moved`. Copying extractor
facts into the key had changed identity despite discovering no ambiguity.

**Decided: ordinary resolution returns a resolver-owned key refinement beside
the outcome.** `RefKeyRefinement::None` preserves the coarse key, regardless
of argument types the extractor recorded.
`RefKeyRefinement::ArgumentTypes(Vec<String>)` supplies the complete vector the
resolver used to distinguish legitimate outcomes. The existing `resolve`
operation remains unchanged, and the combined operation defaults to ordinary
resolution plus no refinement. Only ordinary phase two calls the combined
operation; supertype linking continues to call `resolve`. The pipeline neither
panics nor drops a reference for either refinement.

**Stream C uses that hook only after the authoritative legacy Java pass
returns `AmbiguousOverload`.** A complete `Reference.arg_types` vector then
permits one typed applicability retry. A typed resolution may replace that
ambiguity; a typed ambiguity or any typed miss retains an honestly refined
`AmbiguousOverload`. Legacy resolved, external, local, and every other
unresolved outcome return unchanged with no refinement. Candidate dependencies
are the deterministic union of every identity read by both passes: legacy
order first, followed by each typed-only identity at its first occurrence.

**A resolver also publishes a graph-semantics revision, defaulting to zero.**
Revision zero feeds the established manifest digest to the per-language store
fence byte-for-byte unchanged. A nonzero revision is domain-separated and
folded deterministically into that digest, including for a language whose
manifest digest is empty, so the existing fence forgets only files owned by
that language. Every language stays at revision zero in C1. Java moves to
revision one in Stream C, when it first consumes resolver-owned argument-type
refinement.

Java's finite typed surface includes exact-array use of a varargs declaration
in the fixed-arity phases and zero-tail varargs selection through Java-owned
prefix aliases. It parses integer literal radix, suffix, and legal range,
applies unary numeric promotion from `byte`, `short`, and `char` to `int`, and
compares simple and qualified `java.lang` spellings canonically. It does not
infer return types or user-defined subtype relations.

*Rejected: defer Stream C to 0.2.0 and revert C0.* That slips the Java overload
capability and spends two revert changes.

*Rejected: defer Stream C and keep C0.* That leaves a live key dimension and
schema break with no resolver owning when the dimension is populated.

*Rejected: amend pin semantics to accept the attributed Java rekey.* That
weakens the gate which exposed this defect and makes a later semantic rekey
indistinguishable from an accepted one.

*Rejected: run typed applicability for every call with known arguments.* That
would move already-resolved legacy targets and recreate C0's key churn.

*Rejected: replace the legacy candidate set with the typed pass's probes.*
That would remove invalidation dependencies and leave refined rows stale after
an overload edit.

*Rejected: turn unsupported typed shapes into a new reason.* The taxonomy does
not need another bucket; the legacy `AmbiguousOverload` remains honest.

---

## 2026-07-29 — Java overload narrowing stops at file-local argument types

**Decision:** a Java call or creation stores its complete file-locally-evident
argument vector on `Reference.arg_types`; non-call references and an invocation
with any unknown argument store `None`. Literals, declared names, casts, class
creations, and unary `+`, `-`, or `~` over a numeric literal are included.
Calls, `null`, lambdas, member expressions, array access, and general operators
remain unknown. The resolver reads only the reference field; the temporary
byte-offset side table is gone.

JLS §15.12.2's phase order is preserved: strict fixed arity, loose fixed
arity, then variable arity. Applicability is collected across the receiver and
its indexed supertypes before selection, so an inapplicable declaration on the
receiver cannot hide an inherited or varargs candidate. The first phase with
applicable declarations wins. Conversion-depth dominance selects among that
phase's candidates, then class-over-interface and subtype-owner specificity
break owner ties; incomparable survivors remain `AmbiguousOverload`.

The conversion surface is deliberately finite and fixture-proven: identity;
primitive widening; boxing; unboxing followed by primitive widening; numeric
wrapper to `Number` to `Object`; and `Character`/`Boolean` to `Object`.
No user-defined subtype relation is guessed. Unique callables keep their
existing arity node as the edge target and gain a Java-owned full-signature
alias that forwards to it; overloaded callables retain their full-signature
nodes and set-valued arity marker.

The original Stream C card said no core change was needed because overload
types were already in the node keyspace. That was true for definitions and
false for reference rows: same-arity calls with different argument vectors
collapsed. The plan amendment and C0 corrected the discrepancy by adding
`Reference.arg_types` and `RefKey.arg_types` in a dedicated core landing;
Stream C merged that landing and changes no core file itself.

*Rejected:* putting argument types in `JavaHeader` keyed by byte offset, which
would again let stored row identity disagree with the facts resolution reads.
Also rejected: arbitrary class-subtype guesses, return-type inference,
member/field typing, array element typing, and other written-type-environment
work reserved for Streams H and I. Those shapes stay honestly ambiguous.

---

## 2026-07-29 — reference-row identity carries file-local argument types

Two same-arity calls with the same literal target and enclosing definition can
legitimately resolve to different overloads when their argument types differ.
The reference row previously keyed those sites by arity but not argument
types. Because a row carries one outcome, that collapsed distinct answers:
the first occurrence supplied the stored outcome while later occurrences only
increased its count, even though their edges could name different targets.

**Decided: `Reference` and `RefKey` carry
`arg_types: Option<Vec<String>>`.** `Some(types)` means every argument type is
file-locally evident; `None` means at least one needs inference or the
extractor does not record types. The canonical redb key and edge-pin row hash
include the field. Schema generation 11 wipes older stores so no generation-10
key is decoded under the new grammar.

Every current extractor writes `None`, including Java. This core change is
therefore representation only: no language begins type-directed resolution
here. Java's language-owned follow-up can populate the field and resolve from
the reference itself, so everything its resolver reads is in the row key or
derived from it.

*Rejected: folding types into `raw_target`.* That field is the literal text at
the site; changing its meaning would corrupt source-facing output.

*Rejected: adding the span to the key.* It would distinguish every occurrence
and destroy intentional row deduplication.

*Rejected: demoting a row when collapsed occurrences disagree.* That would
under-report genuinely resolved sites and leave occurrence edges disagreeing
with the stored row rather than fixing the identity defect.

---

## 2026-07-29 — binary releases use cargo-dist, with dispatch as a build-only proof

**Decided: cargo-dist 0.32.0 supplies the release build graph, while
`release.yml` is a reviewed project-maintained customization derived from it.**
It builds the five requested native targets: `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`,
`aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`. The five target release
archive/checksum pairs are
`arthron-x86_64-unknown-linux-gnu.tar.xz` and `.sha256`,
`arthron-aarch64-unknown-linux-gnu.tar.xz` and `.sha256`,
`arthron-x86_64-apple-darwin.tar.xz` and `.sha256`,
`arthron-aarch64-apple-darwin.tar.xz` and `.sha256`, and
`arthron-x86_64-pc-windows-msvc.zip` and `.sha256`. They are release outputs,
not workflow-artifact bundle names. The release also has `source.tar.gz`, its
checksum, and `sha256.sum`.

A `push` matching `v*` is the only publishing event: its host job receives
`contents: write`, uploads every generated asset, and creates the GitHub
Release. `workflow_dispatch` has no publishing condition; it runs `dist plan`
and the full build matrix with root `contents: read`, so it cannot create a
tag, upload a release asset, or create a release. Its multiple temporary
workflow bundles are `cargo-dist-cache`, `artifacts-plan-dist-manifest`, one
`artifacts-build-local-<target>` bundle for each of the five targets, and
`artifacts-build-global`; their names do not assert a one-to-one release asset
mapping.

This is the safest test path available before the workflow exists on the
default branch. GitHub only loads workflow definitions from the default branch
for manual dispatch, so an end-to-end dispatch cannot be run from this worker
branch without first shipping the workflow. The ship-stage proof is therefore:
dispatch `Release` on the merged default-branch commit, inspect the multiple
workflow bundles for the five named target archive/checksum pairs, and verify
that no GitHub Release exists; then, only for the planned release tag, verify
those named pairs are attached to its GitHub Release.

**Selection evidence, checked 2026-07-29.** GitHub's latest cargo-dist release
is stable `v0.32.0`, published 2026-05-22; its release asset checksum verified
locally before its `dist` binary was used. Its workspace declares
`MIT OR Apache-2.0`, ships both license texts, and has a `SECURITY.md` with
private GitHub vulnerability reporting and a security contact. GitHub's
advisory API query for the Rust `cargo-dist` package returned an empty list at
this check. That is a point-in-time vulnerability query, not a claim that no
future advisory can exist. cargo-dist is a CI tool only: this repository adds
no Cargo dependency, no `Cargo.lock` entry, and no product transitive
dependency. The workflow pins every action invocation and cargo-dist's
`github-action-commits` metadata to the verified immutable commits
`actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803`,
`actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a`, and
`actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c`.
Build-time downloads are confined to CI.

The workflow deliberately carries `allow-dirty = ["ci"]` because cargo-dist
0.32.0 cannot represent the project's hybrid tag-push and dry-dispatch policy.
The reviewed deviations are: the `v*` trigger; non-publishing dispatch through
the tag/publishing outputs and host guard; removal of the pull-request trigger
and rust-cache integration; root `contents: read` with `contents: write` only
on `host`; immutable action SHAs; shell quoting and grouped
`$GITHUB_OUTPUT` writes; and the plan/local/global/host job grouping and
handoff. `pr-run-mode = "upload"` remains only because cargo-dist's generated
build condition uses it to enable the dispatch matrix; this workflow has no PR
trigger. On a cargo-dist update, regenerate the baseline, review the complete
diff against this customization and reapply only these documented deviations.
`dist generate --check --allow-dirty` does not prove this customized workflow
is consistent and is not a release-workflow acceptance test.

*Rejected: a hand-rolled matrix and release upload workflow.* It would recreate
cargo-dist's target-to-runner mapping, archive layout, checksums, manifest
handoff, and release asset collection as project-maintained logic. cargo-dist
already supplies that maintained build graph while adding no runtime network
behavior or product dependency.

*Rejected: a release-capable manual dispatch.* A pre-tag test must not create
an external release. Dispatch deliberately cannot reach the host job; the
publishing branch is reachable only through a pushed `v*` tag.

*Resolved after Stream G merged: README binary-install instructions.* Stream E
now documents the five target archive/checksum pairs without disturbing
Stream G's README changes. Stream M retains only the final proof that the
versioned 0.1.0 tag publishes those named assets.

## 2026-07-29 — collision disposition belongs to the stored definition set

Two files declaring C# `N#Shared` as a partial type exposed a split answer:
the cold C# scan reported **0 FQN collisions**, while an unchanged warm scan
and a later full-registry report reported **1**. The node was correct — both
declaration paths survived — and the count was not. The pipeline subtracted
mergeable identities using only definitions extracted in the current event;
an unchanged event had none, while `Store::report` counted the durable
multi-file node mechanically.

**Decided: the store persists each definition site's merge-relevant facts and
a collision disposition for the complete current declaration set.** For every
touched multi-file identity, declarations are read in deterministic
`(file, line)` order and every unordered pair is handed to the language's
`Resolver::mergeable`. The disposition is committed after phase 1 and before
phase 2 restores any file's currency claim, so a completed report is a
function of the stored graph rather than of the last event. A deletion first
withdraws the surviving co-declarers' claims, then reclassifies and restores
them through the ordinary waking path; an interruption therefore cannot
publish the old disposition as current.

The discriminating tests name the identities and sites: partial `N#Shared`
keeps `a.cs` and `z.cs` and reports zero cold, warm, direct-store and
full-registry; field-versus-property `N#Shared::Value` keeps both paths and
reports one cold and warm. A three-site `field, property, field` fixture
reports one because the non-adjacent field pair is incompatible, then reports
zero after that declaration is deleted. This is the reason adjacent-window
comparison is insufficient even when its order is deterministic.

Schema generation moves **9 → 10** and old stores wipe and rebuild. The graph
is a cache; migrating encoded declaration sites and inventing a disposition
for data whose language definitions were not stored would be a guess.

*Rejected: keeping the event-local subtraction.* It is definitionally unable
to answer an unchanged warm event or a later registry track. *Rejected:
checking adjacent windows.* Mergeability need not be transitive. *Rejected:
dropping duplicate declarations or counting only one partial.* Both violate
the never-drop rule and erase queryable source sites. *Rejected: teaching the
store language semantics.* The resolver remains the only authority; the store
owns persistence and tallying of the verdict.

---

## 2026-07-29 — the walk keeps a file's declarations and forgets its references

Wave 2 taught Go to emit type uses, non-call selector reads and
composite-literal keys. Against a 17,873-file / 5,353,211-line Go tree that
took Go references from 595,892 to **1,678,021 (2.82x)** — and peak RSS from
376,092 kB to **830,612 kB, 158.4% of the hard 512 MiB ceiling**. Wall clock
was never the problem: 70.6 s is 13.2 s per 1M lines against a 60 s target.

Measured, not guessed. `perf` is unavailable on the reference box
(`perf_event_paranoid=4`) and no massif/heaptrack is installed, so the peak was
decomposed by a 100 ms `/proc/<pid>/status` sampler plus exact live-byte
accounting that summed every `String::capacity` and `Vec` capacity per field
through glibc's chunk-size rule. That accounting closed to **97.7% of measured
VmRSS**, which is what makes the attribution evidence rather than a model:

| term | of the 813.2 MiB peak |
|---|---|
| the changed set's references, live from the walk until phase 2 | **729.9 MiB — 89.8%** |
| redb page cache and dirty pages | 83 MiB — 10.2% |
| the resolver's own indices | 7.6 MiB — 0.9% |

The shape mattered more than the total: RSS climbed monotonically to 729.9 MiB
**with the database still untouched**, and minor faults stopped at t≈38 s. Phase
2 never asked the kernel for another page — it ran inside memory the walk had
already committed. The peak was not a spike to smooth; it was the whole parsed
tree, held.

**Decided: `ScannedFile` keeps a file's path, hash, header and declarations, and
not its references. Each later phase reads the file again.** References
outnumber declarations 23 to 1 (1,678,021 against 72,362) and cost 136 bytes
plus 5.7 heap allocations each; declarations cost 13.3 MiB for all of them.
Phase 1 needs every changed file's declarations before it names anything, so
those stay. Nothing needs every file's references at once, so nothing holds
them.

**That 13.3 MiB is one term of the retained set and not the whole of it, which
this entry originally left to be inferred.** Sampled at 200 ms, the shipped
build's walk ends at **110,896 kB with the store still at 1.1 MB** — nothing
written yet
— against a **10,240 kB** fixed cost measured on a two-file tree. So the walk
retains ~100 MB, not 13 MB: a `String`, a `PathBuf`, a 32-byte hash and a
language header ride beside every changed file's declarations, and the walk
keeps a second path per *owned* file in `owned`. It is retention rather than
recycled slack, which is checked and not assumed: forcing every allocation of
128 kB or more through `mmap` — `MALLOC_MMAP_THRESHOLD_=131072`, so the big
per-file transients go back to the kernel on free — moves that plateau to
111,604 kB, 0.6% the wrong way. The remaining terms have not been decomposed
per field. Whoever tunes this next should measure before assuming the
declaration count is the term to shrink; the peak is linear in the file count
too.

Re-extraction cannot change an outcome, and the enforcement is structural
rather than argued: `Extractor::extract(&self, rel_path, source)` takes no
probe, no config and no other file, so the same bytes give the same facts.
`scan_file` already re-read every woken file on exactly that assumption, so
this generalises a path the graph already rested on. The bytes are re-hashed
on the second read — a file that moved under the scan is routed to the existing
`stale` path rather than resolved against declarations its source no longer
makes.

| change | peak RSS | wall | of ceiling |
|---|---|---|---|
| the wave, as measured | 830,612 kB | 70.59 s | 158.4% |
| `shrink_to_fit` the extractor's two vectors | 778,328 kB | 70.57 s | 148.5% |
| **and stop holding the references** | **286,872 kB** | **109.62 s** | **54.7%** |

The last row is the worst of nine runs of the shipped build, which spanned
284,612–286,872 kB and 106.47–109.62 s; a single run is not a peak. The first
row reproduces at 832,468 kB / 69.54 s on a re-measurement of the same commit.

**286,872 kB — 54.7% of the 524,288 kB ceiling, and below the 376,092 kB that
v0.0.1 needed for a third of the references.** The cost is 39.0 s of wall clock,
one extra parse per file: **20.5 s per 1M lines against a 60 s target** on that
Go tree. That is the trade taken deliberately — the ceiling is hard and the
timing is a target.

**The margin is not the same in every language, and TypeScript no longer has
one.** Per 1M lines, median of three cold runs of each build on the reference
hardware:

| corpus | lines | before | after |
|---|---|---|---|
| `go/caddy` | 97,148 | 19.4 s | 32.6 s |
| `javascript/fastify` | 69,250 | 19.2 s | 37.1 s |
| `javascript/express` | 21,231 | 24.5 s | 42.9 s |
| `python/django` | 161,112 | 18.4 s | 42.7 s |
| `python/flask` | 17,019 | 20.0 s | 43.5 s |
| `java/commons-lang` | 189,376 | 24.5 s | 44.4 s |
| `java/gson` | 48,657 | 27.1 s | 48.7 s |
| `typescript/vue-core` | 151,099 | 36.7 s | **70.1 s** |
| `typescript/zod` | 68,361 | 45.3 s | **82.5 s** |
| the 5.35M-line Go tree | 5,353,211 | 13.0 s | 20.2 s |

The extra parse roughly doubles all of them, so the language that started
nearest the target is the one that crossed it: **both TypeScript corpora now
index above 60 s per 1M lines.** The earlier reading of this entry — that the
tree could grow 2.9x before timing bound — was the Go number generalised, and
it is wrong for TypeScript, which is already past the target. Timing is a
target and not a gate, so this ships; a missed target is recorded rather than
inferred, here and in `README.md`. TypeScript extraction being 2x the cost per
line of every other track, before and after, is a separate piece of work and
not this one.

Every resolution number is unchanged, checked rather than asserted: all 29
corpus gates and all 15 pin comparisons were re-run against the committed
baselines and pin files, `44/44` exit 0, and the **complete stdout of all 44 —
every rate, every reason tally, every `held/appeared/vanished/moved` count — is
byte-identical to the same 44 runs on the parent commit**. `git status
--porcelain baselines/ pins/` is empty. This was a change of representation; it
was required to be invisible to the graph, and it is.

*Rejected: capping, sampling or summarising references to fit.* This is the
predecessor's exact failure and the resolver-never-drops non-negotiable exists
because of it. A reduction that lowers RSS by losing data is worse than
shipping over the ceiling.

*Rejected: inline small strings (`CompactString` or equivalent), worth a
projected 161.5 MiB.* 5,291,802 of 6,655,300 string allocations are ≤24 bytes
and cost 161.5 MiB chunked. Real, and unnecessary: the change above already
lands at 54.7% of the ceiling. It buys a dependency and a mechanical type
change across every track for headroom that is not needed, which fails the
"check whether a simpler local change is enough" test.

*Rejected: jemalloc, worth a **measured** 38.3 MiB and 9.9% less wall clock.*
793,548 kB / 63.6 s against 832,740 kB / 70.6 s, report byte-identical. The
cheapest line in the whole investigation and still declined for the same
reason: a global allocator is a dependency and a platform-portability question
for a saving the structural fix has made irrelevant. Recorded here because it
was measured and would be the first thing to reach for if the ceiling ever
binds again.

*Rejected: spilling the extracted facts to a temporary file instead of
re-parsing.* One extraction instead of two, but it needs a serialisable
`L::Header` on every track, a temp file to create, fsync, find and delete, and
a new failure mode when it cannot be written. Re-extraction reaches the same
peak with no new state and no new file.

*Rejected: `shrink_to_fit` alone.* Kept, but only where something is held —
and that placement was itself a review finding. Measured at **52,284 kB** on
the build that still retained every file's references, half the 102.5 MiB the
live-byte accounting projected, because glibc returns a shrunk allocation to
its own arenas rather than to the kernel; that gap is the standing reminder
that live bytes and RSS are different measurements. Once the references are no
longer retained, shrinking them on the walk's path is dead work — they are
dropped at the end of the statement that makes them — so the walk shrinks the
declarations it keeps and nothing else, and `reread` shrinks the references it
hands out, because those live until the phase that asked for them is done.
Neither placement is visible in the total: removing the references' shrink
entirely measured 278,444–286,660 kB over three runs on the 5.35M-line tree
against 284,612–286,872 kB over nine with it — overlapping ranges, and the
widest single-run excursion in either direction (8,216 kB) is larger than the
difference between them.

**The regression guard is a mechanism test, and `tests/rss_ceiling.rs` says so
in its own comment rather than implying CI covers the ceiling.** CI cannot run
this measurement: the tree it is stated against is not in the corpus
repository.

The first version of this entry went further and said a threshold on *any*
corpus tree would have passed on the code the guard rejects, resting on a
single-run 300 kB delta on the largest Go corpus. Three runs of each build say
otherwise on both counts. On Go the two builds are indistinguishable, which is
the weaker and truer version of that claim — `go/caddy` 55,752–55,952 kB before
against 55,808–56,320 kB after, `go/codeiq` 46,184–46,208 kB against
46,168–46,296 kB, overlapping ranges with each build's own spread as large as
the difference between them — so the direction of the original 300 kB is not
reproducible and it should never have been stated as a measured regression. But
three non-Go corpora *do* separate the builds: `python/django` 68,908–69,020 kB
against 59,160–59,288 kB, `javascript/fastify` 26,112–26,368 kB against
19,688–20,080 kB, and `javascript/express` 16,172–16,336 kB against a flat
14,592 kB — gaps of 9,620 kB, 6,032 kB and 1,580 kB, where no build's own
spread on any of them exceeded 512 kB. Their fixed cost is small enough that
the retained references cleared it, which the Go corpora's does not.

**So a corpus proxy exists and is still not the gate.** It would separate the
builds by ~10,000 kB where the tree the ceiling is stated against separates
them by 545,596 kB, and a peak-RSS threshold is a statement about the hardware
and the allocator — the reference hardware is 2 vCPU under `taskset`, and CI
runners are not it. Asserting a memory number where the memory is not the
reference machine's buys a check that fails for reasons unrelated to the code.
Recorded rather than built, so the option is on the record with its own
measurements.

The test therefore pins the mechanism: a Go file is extracted exactly twice and
a Java file exactly three times, counted through the public `scan` entry with a
wrapped extractor. It fails with `1` on the parent commit, which was verified
by running it there. Its blind spot is named in its own comment — putting the
references back into the retained record while *keeping* the re-read leaves
both counts unchanged and the test green — because a guard whose limits are
undocumented is read as covering more than it does.

---

## 2026-07-29 — the second re-pin: a docs-only merge moves no edge, and the pins say so in bytes

Main's docs-truth work — retracting the call-graph claim, publishing the rate's
denominator, correcting documented numbers — is prose, README tables and one
display string (`Lang::rate_scope`, "call-graph resolution" → "call, import and
type-use resolution"). Nothing in it touches resolution. That is a claim, and
the pin gate is the only thing here that can settle it, so it was checked before
it was believed: all fifteen corpora were compared against their committed pins
*before* anything was regenerated.

**Every corpus: `0 appeared, 0 vanished, 0 moved`** — 80,272 held rows over
fifteen pin files. Regenerating then left every one of the fifteen files
differing from its committed version in exactly two lines, both of them the
provenance header: `commit =` and the `arthron pin …` command inside the
comment. Not one body byte moved. The first re-pin could only say that no
*pre-existing* row was re-aimed while thousands appeared; this one says the
stronger thing, that the resolver's answer over 80,272 edges is byte-identical
across the merge. A null result is the result a targeted gate should produce
for a change that targets nothing, and it is worth writing down precisely
because the counting gate would have produced the same silence for a merge that
re-aimed every edge in the tree.

**Decided: a re-pin whose body does not move is still written, at the new
commit.** The header records the tree the pins were last verified against, and
after absorbing main that is the merge commit, not its parent.
*Rejected:* skipping the write to keep the diff empty. That buys a clean diff
by letting the header name a tree the file was not checked against. `corpus`
and `commit` are provenance — printed, never verified — which is only safe
while the regeneration command shown in the header is the one that was actually
run; a header left behind is the first step to a pin file nobody can date.

**A number this branch had already got wrong, found by re-measuring instead of
re-reading.** The `Unreleased` entry introducing `arthron pin` still said
*eleven tier-1 corpora — 76,792 rows over 18,325 distinct targets*, the count
from before the four probe corpora were pinned one commit earlier. It ships
fifteen files: 80,272 rows over 18,687 targets, 3.1 MB, and the CI comparison
is fifteen cold scans at 7.4 s wall. Corrected in `CHANGELOG.md` against
measured values — the method was validated by re-deriving `76,792` and `18,325`
exactly from the pin set as it stood at that commit. The mutation-proof
paragraph in the same entry is deliberately left alone: *275 and 266 moved
edges, all 21 pin tests green on revert* is the record of a run that happened
when there were 21 such tests, and re-numbering it to today's 35 would be
claiming an experiment nobody re-ran.

---

## 2026-07-29 — the first re-pin: probe corpora are pinned, and no target moved

The wave's resolver work landed on a branch that already pinned every resolved
edge's target, so this is the pin mechanism's first real exercise: absorb main,
regenerate all of it, and account for every row that moved.

**No pre-existing pinned row changed target — on any corpus.** Across the
eleven corpora pinned before the merge the verdict was `0 moved, 0 vanished`
throughout, and the target dictionaries only grew (no corpus dropped a target
name). All movement was rows that *appeared*: `go-codeiq` 6383 → 7692 keys,
`go-caddy` 7366 → 7592, `typescript-zod` 8377 → 10289; the other eight were
byte-identical in their bodies. That is what the Go field-access work and the
ECMA specifier work should look like — new edges where there were none, and not
one existing edge re-aimed — and it is the claim only this gate can make, since
all three corpora also moved the four counted integers (`go-codeiq` resolved
8016 → 9794) and the counting gate cannot tell a new edge from a re-aimed one.

**Decided: probe corpora carry pin files, on the same rule as any other
corpus.** The rule is the one already enforced programmatically — every tier-1
corpus with a baseline has a pin file — so the four probe corpora that landed
with the wave inherit it rather than being argued about case by case.
*Rejected:* excluding them as too small to be worth a file. That reads the
value backwards. A probe corpus exists because each file in it encodes one
resolver contract that was got wrong once, so its rows are few and every one is
load-bearing — a higher pin density than any real tree here. They cost
kilobytes: 10, 6, 5 and 12 rows for java, javascript, python and typescript.

**A defect this re-pin caused and caught.** The per-corpus scan tests indexed
the pin table by *position* (`check(PINNED[5].0, PINNED[5].1)`), so inserting
the probe corpora mid-table silently re-aimed every test after the insertion
point: `javascript_express_edges_are_where_they_were_pinned` began scanning
`corpus/java/probes`, and the suite stayed green because every corpus still
agreed with its own pins — each was simply checked under another test's name.
A test whose name points at the wrong corpus is the pin gate's own defect one
level up. Each test now looks its corpus up by the pin path it names, and a new
test refuses a `PINNED` row that no test scans, since the existing coverage test
only matched committed files against the table and would pass on a pin file
nothing ever compared. Verified by mutation: dropping one scan test fails the
new check by name.

---

## 2026-07-28 — 0.0.1 published

`arthron 0.0.1` is on crates.io, tagged `v0.0.1` (signed). Every ratified
publish condition was met and verified, not assumed: all twenty-five corpus
gates green on main; CI green on Linux, macOS and Windows; docs rewritten
against the committed baselines (every quoted rate re-derived, every console
block asserted byte-identical to real output); the perf evidence recorded in
this log; `cargo publish --dry-run` clean with the package inspected — 125
files, corpus/baselines/docs/tests/.github verified absent.

**What the first 3-OS run caught, recorded because both are classes:**

- *The gate wrote a file it could not read back* (Windows). `write_baseline`
  recorded the corpus path with `\`; `parse_baseline` refuses escapes by
  design, so the next run exited with a usage error and an empty document.
  Fixed at the single construction point, and the write-side validator now
  refuses everything the reader refuses — a writer must never outrun its
  reader.
- *A fixture the platform cannot produce is not a skipped test* (macOS).
  APFS refuses non-UTF-8 filenames, so the non-UTF-8-path test is gated to
  Linux with the reason in the comment.

**The packaging near-miss, recorded as a standing rule:** an unanchored
`include` pattern in `Cargo.toml` follows gitignore semantics and matched at
any depth — cargo followed the corpus symlink and packaged twenty-one
license files from the private corpus repository. Caught by inspecting
`cargo package --list`, fixed by anchoring every pattern. The list is
inspected, never trusted.

**Rejected for 0.0.1:** cargo-dist binary releases (rides a later 0.0.x —
crates.io is the distribution the conditions named); publishing to public
npm (GitHub-Packages reservation stands).

---

## 2026-07-28 — Tier 2 complete: fourteen tracks, twenty-five gates, every number measured

The last eight tracks landed — batch 3 (Swift, C++), the `.h` amendment, and
the six best-effort languages. Every registered language now has a live
track, a measured committed baseline, and a CI gate.

| language | corpus | resolved | external | unresolved | rate |
|---|---|---|---|---|---|
| Swift | alamofire 7595cbc | 40 | 130 | 0 | **100.0%** |
| C++ | fmt 1be298e | 127 | 254 | 18 | **87.6%** |
| Dart | collection dec28c1 | 75 | 49 | 0 | **100.0%** |
| Elixir | plug 9fa11c8 | 116 | 55 | 1 | **99.1%** |
| Haskell | aeson (pinned) | 278 | 796 | 0 | **100.0%** |
| Lua | busted 56e6d68 | 99 | 0 | 153 | **39.3%** |
| Bash | bats-core (pinned) | 0 | 0 | 6 | **0.0%** |
| HCL | terraform-aws-vpc (pinned) | 23 | 1 | 0 | **100.0%** |

**The `.h` amendment, executed as reserved.** The C++ track first landed at
**3.4%**: fmt is header-dominated and every header is `.h`, the extension
the tier-2 registration left unclaimed. The track refused to widen the
claim mid-wave and measured the counterfactual instead; the follow-up
commit claimed `.h` with the measurement in hand — 33 → 54 readable files,
3.4% → **87.6%**, the 17 remaining misses all the deliberately-unvendored
gtest includes. A pure-C repository's headers parsing under the C++
grammar stays an accepted, documented risk until a C track exists.
*Rejected:* widening inside the go-live commit (an extension claim is its
own decision, measured separately).

**A 0.0% baseline is a working gate.** bats-core computes every `source`
target at runtime (`BATS_ROOT=${BATS_PATH%/*/*}`), so Bash's rate line
holds 0.0% honestly and the drift checks on the other columns are what
make it un-gameable; the track's value is its function-definition census.
Lua's 39.3% carries the same honesty the other direction:
`ProjectLayoutUnknown 53` is `require 'busted'` matching two real files
(`?.lua` vs `?/init.lua`, decided by `package.path` order at runtime) —
recorded as genuinely ambiguous, never guessed.

**The laundering class was caught twice more before merge.** Dart's review
found `path:` dependencies — packages the tree itself contains — classified
`External`; fixed by linking through the dependency location, following the
Rust track's `Dep::Local` precedent. Elixir composed nested `defmodule`
names from the start (the C# lesson, applied from the trap checklist in
this log rather than rediscovered).

Merge mechanics for the whole fanout: fourteen tracks landed through
serial merge trains over two shared append-only files (`sg.rs`,
`tests/baselines.rs`); every conflict was the same append-append shape,
every resolution verified by the full suite plus all committed gates
before push. The frozen core was breached zero times; the one core change
a track needed (`denominator_shrank`) landed as its own PR.

---

## 2026-07-27 — Batch 2 tier-2 tracks: C# 100.0%, Kotlin 89.5%, Scala 42.3%; the gate learns to see a dropped reference

| language | corpus | resolved | external | unresolved | rate |
|---|---|---|---|---|---|
| C# | serilog 6d9fc0b | 53 | 36 | 0 | **100.0%** |
| Kotlin | okio 6604edb | 683 | 1,136 | 80 | **89.5%** |
| Scala | upickle 87e0b24 | 267 | 0 | 364 | **42.3%** |

The spread is the import models, not the tracks. Serilog resolves through
three `GlobalUsings.cs` files and SDK implicit usings, so its per-file `using`
surface is tiny and fully in-repo. okio's 1,136 external imports are the
multiplatform stdlib surface. upickle's 42.3% is an honest floor:
`UnknownPackage 309` — without reading `build.mill`'s dependency list the
resolver cannot tell an external package from a mistyped in-repo one, so it
refuses to guess `External`; reading the build definition is the recorded
promotion path.

**The core change this batch forced (landed as its own PR per the
frozen-core rule): the gate refuses a shrinking denominator.** The C#
review noticed that at a 100% baseline a *dropped* `Resolved` row was
invisible to the gate, and at any baseline a dropped `Unresolved` row read
as an improvement — rack's 291/50 drifting to 291/49 passed. The gate now
fails with `denominator_shrank` when measured `resolved + unresolved` falls
below the baseline's; growth stays legal. This also mechanically enforces
the capability re-base rule: a landing that reclassifies `Unresolved` →
`External` must re-base, not compare.

**The batch-1 laundering shape recurred and is now a named class.** Nested
`namespace Alpha { namespace Beta { … } }` declared `Beta`, not
`Alpha.Beta`, so a `using` of an in-repo namespace was classified
`External` — the resolver's own bug landing outside both rate terms. Fixed
with namespace composition; the census caught two more C# grammar
surprises (tree-sitter flattens `params` parameters; `using static`
scoping) that count-based tests would have slept through.

---

## 2026-07-27 — Wave 3: the RSS ceiling holds — 729 MiB becomes 337 MiB

The robustness wave closed the benchmark entry's hard-gate failure.
Kubernetes (1,789,247 lines) cold-scan peak RSS: **729.0 MiB → 337.1 MiB**,
66% of the 512 MB ceiling, six runs spanning 0.2%. No timing regression
(~17 s per 1M lines against the 60 s target); warm RSS improved 13%.

**What was corpus-sized:** the whole-event redb write transaction (+236 MB
at peak — redb holds a transaction's dirty pages until commit), redb's
default 1 GiB page cache (against a 257 MB store nothing was ever evicted,
so the floor became the database's size), and phase-2 batches that borrowed
the entire extracted-fact set. **What bounds it now:** every phase commits
per 500 files (the design doc's measured 60 ms/500-file figure), the cache
is capped at 96 MiB on both open paths, and phase 2 consumes facts per
file. Java's marginal slope fell 2.38 → 0.74 kB/line, moving its projected
ceiling crossing from ~264k to ~658k lines.

**Byte-identity was proven at graph level, not tally level:** pre- and
post-change binaries scanned five corpora into separate stores and every
blake3 digest of the full snapshot — files, nodes, rows, edges, candidates,
supertypes — matched.

Also landed: a held store is refused by name instead of deadlocking (both
open paths, bounded tests); unreadable and undecodable files are reported
per file, never dropped and never fatal; a stepped-over file loses its
currency claim; an absent root fails cleanly.
*Rejected:* chasing the 1 s warm target in this wave (warm cost is per-file
re-read and re-hash, orthogonal to batching); raising the ceiling.

---

## 2026-07-27 — First tier-2 tracks live: Rust 98.0%, Ruby 85.3%, PHP 67.9%

Three tracks landed in parallel from one batch, each measured against its
vendored corpus with `gate --rebase`, each now enforced in CI. A tier-2 rate
is an **import-resolution rate** — definitions, structure, and imports, no
call edges — and is never comparable to a tier-1 rate or to another
language's.

| language | corpus | resolved | external | unresolved | rate |
|---|---|---|---|---|---|
| Rust | ripgrep e89fff8 | 649 | 411 | 13 | **98.0%** |
| Ruby | rack e1f22fd | 291 | 1 | 50 | **85.3%** |
| PHP | guzzle 3aeea04 | 360 | 265 | 170 | **67.9%** |

PHP's 170 unresolved are all `ModuleNotFound`: `use` statements naming
sibling packages under the same vendor namespace that sit outside the
corpus snapshot — an honest floor of the snapshot's scope, not a resolver
gap. Rust's 13 are 11 `AliasCycle` (the `pub extern crate grep_printer as
printer` re-export chains) and 2 `NoMatchingDefinition`.

**What the adversarial reviews caught before merge, recorded because the
class recurs:**

- *An in-repo crate laundered as `External`* (Rust, high): the resolver read
  only literal `path =` dependencies, so `foo = { workspace = true }` — the
  standard spelling since Cargo 1.64 — sent a sibling crate outside the
  measurement entirely. `External` sits outside both rate terms, so the
  reference vanished rather than failing; the corpus itself never spells it
  that way, which is exactly why the reviewer probed it with fixtures.
  Fixed: workspace dependency tables are now resolved from the root
  manifest.
- *A wrong edge beats a miss* (Rust, medium): a `path` dependency named like
  a local module shadowed the local, binding the reference to the wrong
  crate. Fixed against rustc's own binding order, proven with a
  discriminating fixture.
- *A census, not a count* (Ruby, high): the corpus acceptance asserted the
  tally but not the definition census, so an extractor bug dropping 566 of
  633 methods kept every test green — proven by mutation. The acceptance
  now pins the full per-kind census plus nine named definitions with their
  declaration lines.

**Unexercised mechanisms, recorded in the corpus provenance rather than
implemented blind:** ripgrep contains no `#[path]` attribute and no
`workspace = true` (the latter now implemented anyway, fixture-proven);
guzzle contains no `use function`/`use const`, so PHP's global-fallback
rule remains unexercised until a corpus exercises it.

---

## 2026-07-27 — Tier-2 languages registered as disabled tracks; each family is its own domain

**Decision:** the 14 ratified tier-2 languages enter the model as `Lang`
variants with append-only wire codes 5–18 (Cpp, CSharp, Kotlin, Swift, Ruby,
Php, Rust, Scala, Dart, Elixir, Haskell, Lua, Bash, Hcl — codes 0–4
untouched) and as disabled registry tracks. A disabled track owns no files,
so the change reads nothing new and moves no number; the registry test that
a mixed-language scan never changes Go's tally passes unchanged. Every
grammar was verified against the pinned ast-grep 0.44.1 `SupportLang` enum —
none invented.

**Each family gets its own `Domain`.** Kotlin and Scala are deliberately
*not* folded into `Jvm`. Sharing a domain is a measurable capability claim —
it asserts a `.kt` import can name a `.java` definition in one reference
space, the way `.ts`/`.js` provably share module resolution. Nobody has
measured JVM cross-language linking here yet; a domain can be widened later
by evidence, but a wrongly shared identity space silently mints
cross-language edges. `Domain::Cxx` is named for the family so C can join it
if C support ever lands.
*Rejected:* `Kotlin → Domain::Jvm` by analogy with EcmaScript (the analogy
imports a capability instead of measuring one).

**Deliberate non-claims, asserted in tests rather than left implicit:**
`.c` and `.h` are unclaimed — a C file parsed under a C++ grammar is the
wrong language, and a measured K&R sample misparses under the C++ grammar;
`.bats` is unclaimed — the shell grammar *misreads* it (zero error nodes,
but `@test` blocks come back as ordinary commands, not functions); `.hcl`,
`.tfvars`, `.sbt`, `.zsh` stay unclaimed until a go-live commit measures
them. Widening a disabled language's extension list changes nothing a scan
reads, so the first honest moment to claim an extension is the commit that
parses it.

**`arthron gate --language <registered-but-disabled>` refuses before
scanning** — usage error, exit 2, no store created, and the message lists
only what this build can gate.
*Rejected:* scanning first and failing on the empty tally (burns a full
cold scan and prints a report line that looks like a measurement).

---

## 2026-07-27 — A framework rule may mint nodes, in a framework-owned namespace that no language tally counts

**Decision:** `fw:<framework>/<kind>#<key>` — `fw:django/route#myapp:detail`,
`fw:spring/property#app.order.timeout`, `fw:next/route#/blog/[slug]`. A route,
a property key and a settings key are each a thing a reference can name whose
identity is stable under unrelated edits, which is the node rule's test; none
of them is a definition, a module or an external package, which is the closed
list `CONTEXT.md` gives. They are stored in a framework table, hashed with a
framework discriminator, and never entered in `NODES`.

**Rejected:** synthesizing a direct target→consumer edge — making
`reverse("admin:login")` point straight at the view — which loses the route as
an object, cannot answer *"what routes exist"*, and produces a many-to-many
edge explosion when several sites name one route. **Also rejected:** quietly
adding a `NodeRecord` variant, which is the same change made invisible.

---

## 2026-07-27 — FrameworkFact spec self-ratified against the framework-edge decision

Three framework case studies — Django (29 numbered cases plus four findings),
Spring (30), React/Next (30) — were written against the vendored corpora and
the shipped graph, then synthesized into one language-neutral contract. Per
the production-readiness ratification, the spec self-ratifies by quoting the
framework-edge decision verbatim and verifying every clause of its own design
against it; the check found and fixed **four spec bugs** before finishing,
each of which would have violated "never moves a language's rate" (framework
probes in the shared candidates index; a framework `External` materialising
into the language node table; a snapshot format without a framework half; an
inherited fence rule that would leave stale framework edges behind).

**What the contract fixes across the studies' three conflicts:**

- Every framework edge declares, machine-readably, what it adds —
  re-provenancing an already-resolved language edge with dispatch semantics,
  or recovering a target from a string literal that was never in any
  language denominator. Reported as a column, never merged.
- The framework layer carries the same never-drop contract one layer up:
  every site a rule matches is `Resolved`, `External`, or `Unresolved` with a
  reason — including policy reasons no rule improvement can change, because
  arthron does not execute code or touch the network. Policy counts are
  drift-gated the way `LocalBinding` is.
- Per-framework coverage is a rate over sites, per rule family, beside — never
  inside — the language rates. The mechanical enforcement is a byte-identical
  per-language-tally test with the framework layer off versus on.

**Honest limit, recorded rather than hidden:** the framework rate cannot
detect a missing rule, because its denominator is sites the rules matched.

**Rejected:** framework rules as language-extractor extensions (the extractor
is forbidden from linking, and a framework spans languages); a fourth
language-level `Outcome` variant for framework results (the three-variant
contract is not widened — the framework layer has its own).

---

## 2026-07-27 — Reference-hardware benchmark: the RSS ceiling fails at 1.8M lines

First full benchmark on the reference envelope (2 vCPU via `taskset`, hard
ceiling < 512 MB RSS, timing as targets), per the perf ratification:

| corpus | lines | cold wall | cold s/1M | warm no-change | peak RSS cold | peak RSS warm |
|---|---|---|---|---|---|---|
| kubernetes v1.36.3 (Go) | 1,789,247 | 30.15 s | 16.85 s | 2.75 s | **729.1 MiB** | 133.4 MiB |
| commons-lang (Java) | 189,376 | 4.04 s | 21.33 s | 0.27 s | 339.1 MiB | 33.8 MiB |

**Verdicts:** cold RSS on kubernetes **fails the hard gate at 1.42× the
ceiling** — the first hard-gate failure in the project. Cold throughput passes
with 2.8–3.6× margin against the 60 s/1M-line target. Warm no-change misses
its 1 s target on kubernetes (2.75 s) — a finding to explain, not a failure,
exactly as the ratification anticipated for hash-walking a 1.8M-line tree.
Warm RSS passes everywhere. Measured cause: scan memory grows linearly with
corpus size because the cold scan holds every node id and payload in memory;
Java's per-line slope is ~3.5× Go's. Timing numbers were taken on a loaded
box and are upper bounds; RSS is load-independent and the failure is real.

**Decision:** the robustness wave leads with bounding cold-scan memory, and
re-running this benchmark until the RSS gate passes is that wave's acceptance
test. **Rejected:** raising the ceiling — reference hardware is the promise,
not the developer's box. **Also rejected:** promoting the warm-timing miss to
a failure — timing was ratified as a target precisely because runners are
noisy; the RSS ceiling is the hard line.

---

## 2026-07-27 — The Java review round: an erased type frame is still a frame

An adversarial review of the Java track produced eleven findings. Every one
was reproduced by a test written before its fix and none was rebutted; the
Java resolution rate moved from **0.6525 to 0.6640** on commons-lang
(resolved 38,388 → 38,940, unresolved 20,440 → 19,708, external 68,582 →
68,333, local-binding 2,048 → 2,062). What follows is what changed shape, not
the list — the commits carry that.

**An anonymous or local class is not a node, and it is still a scope.** This
was the round's only wrong-edge finding and the only one that mattered more
than the rate. The extractor walks past an anonymous `class_body` when it
looks for a reference's edge source, which is right — the anonymous class has
no canonical name (§6.7) so the edge must start at the nameable member around
it. The resolver then read that same answer back as "the type chain this site
sits in", which is a different question with a different answer: §15.8.3's
`this`, §15.11.2's `super` and §15.12.1's unqualified invocation all search
the innermost enclosing *type declaration*, and the anonymous class is one.
So `super.m()` inside `new Base(){…}` resolved against the *enclosing named
class's* `extends` clause and produced an edge to a method on an unrelated
type; an unqualified `m()` that the anonymous class itself declares linked to
the enclosing class's same-named method; `this.f` linked to the enclosing
class's field. Three wrong edges from one erasure, and wrong edges are worse
than misses.

The fix records the frames the node rule erases — anonymous class bodies,
enum-constant bodies, local classes, and any type declared inside one — with
their byte extent, the supertype they name, and the member keys they declare.
Edge *sourcing* is untouched. A member the frame declares itself is
`LocalBinding`: it is real, and it is by design not a node, which is the same
judgement the node rule already makes for the local class it belongs to. A
member the frame *inherits* now resolves to the supertype that declares it,
which the erasure had been hiding — the fix recovers more edges than it
removes. *Rejected:* declining outright on every `this`/`super`/unqualified
reference inside such a frame, which the review proposed; it removes the
wrong edges and the right ones together. *Rejected:* a flag on `Encloser`
saying the innermost frame was erased — that is a core change, and the
extents are a per-file fact the track can carry itself.

**§6.5.5.1 tier 1 says "including inherited ones", and it meant it.** Simple
type-name resolution consulted only the types the *file* declares, so `State`
inside `class Sub extends Base` — with `Base` indexed, in the same package,
one `extends` hop away — was `NoMatchingDefinition`. That is the one bucket
reserved for meaning *our* bug, so a missing feature was being reported as a
missing definition. Tier 1 now walks the enclosing types' supertype closure
for a member type, the same closure member lookup already walks, reachable
for the same reason and no further (H-01). Largest single contributor to
commons-lang's `NoMatchingDefinition`, which fell 298 → 161.

**A receiver typed by a type variable is a lookup, not a failure (X-07).** The
declared type of `T value` is `T`, which named no type, so `value.tag()` was
`NoMatchingDefinition` while the type use `T` one line up was `LocalBinding` —
the same fact classified two ways, and the one that left the denominator was
the one that inflated. The extractor now records a type parameter's *bound*,
which is written in the same file, and the receiver resolves against it; an
unbounded parameter erases to `Object` (§4.6) and its members are external.

**A member name is not a member.** A method reference (C-08) and a
single-static import (I-04) both name a *member name* rather than one
declaration, and neither site states an arity — so the resolver probed the
bare name, which is the *field* key, and reported `AmbiguousOverload`
whatever came back. Zero method references resolved in all of commons-lang,
and an import naming something absent was indistinguishable from one naming
five overloads. Both now walk the member-name group by arity to a bounded
depth: one declaration is one edge, two or more is the discrimination
`AmbiguousOverload` names, none is the same honest miss any other member
lookup reports. The bound is 8; commons-lang measures identically at 2, 4, 6,
8 and 12, because a name someone types at a call site is short. *Rejected:*
probing to §8.4.1's real ceiling of 255 — half a thousand probes per import,
recorded in the invalidation index, for answers the corpus says are not
there. *Not fixed, and recorded as a core gap:* the case study's §13 names
`TargetTyped` for exactly this shape and `UnresolvedReason` has no such
variant, so a method reference whose overload set is genuinely plural still
reports `AmbiguousOverload` — "we compared candidates and could not choose"
about a set that has no arity to compare at.

**A name is attributed the same way in value position as in type position.**
`java.util.List` was `External`; `java.util.Objects.requireNonNull(…)` was
`AmbiguousName`, three lines from the code that decides the first. §6.5.5
gives both the same split — last segment a simple type name, qualifier the
thing that names it — so both now take it. And the split now asks a question
it never asked: if the qualifier *is* a package this repository declares, the
symbol table's opinion is complete and the type is absent, which is
`NoMatchingDefinition`. Calling that `External` let a definition that should
exist here leave both terms of the rate. Measured on commons-lang: the first
moves six occurrences (`AmbiguousName` 6 → 0), the second moves **none at
all** — a library that writes no fully qualified names and has no sibling
module is exactly where neither bites. Both are routine in a Maven reactor,
which is the layout P-02 and P-06 exist for, so they are fixed on the
argument rather than on this corpus's evidence. *Rejected:* treating any
*prefix* match against a declared package as in-repo — `com.acme.ext.Foo`
with `com.acme` declared and `com.acme.ext` a separate artifact is genuinely
external.

**Two identical branches, one of them throwing away a resolvable lookup.**
`field.tag()` resolved and `this.field.tag()` reported `NeedsTypeInference` —
documented as "the receiver is a name with no declared or annotated type",
about a name whose type is written on line 3. `this.f.m()` now reads the same
declared-type environment (X-02) the bare form does.

**C-05: `new Iface(){…}` invokes `Object#<init>()`.** An anonymous class
implementing an interface does not invoke a constructor of the interface
(§15.9.5.1) — and every in-repo class carries one at probe time, because
§8.8.9's implicit constructor is synthesized (D-10). So a creation site with
a class body whose owner is in-repo and whose constructor is missing has
named an interface, and the honest target is external. Measured: 108
occurrences, 107 of them out of `UnindexedSupertype`.

**A reference is a site in one file, and recovery invents sites.**
tree-sitter's error recovery read a `type_identifier` out of a fuzz-corpus
string literal whose control bytes break the literal: one row, 405
occurrences, `External("$$")`. The extractor already knew `ERROR` nodes occur
— it detects them to recover JEP 511's module imports — but nothing stopped a
reference from being emitted inside one. The gate baseline was defending 405
references that do not exist, which is the whole reason this is recorded
rather than filed as a nit: **a baseline that defends phantoms is not a
baseline.** Ancestors only; an `ERROR` elsewhere in a file says nothing about
a node that parsed.

**`NeedsReceiverType` was documented as Java's honest floor and is
structurally absent.** Its definition is the case where the receiver's type
*is* stated and *is* in the repository — which this resolver looks up rather
than reports (X-02). Two comments claiming otherwise were wrong; the reason
is correct and the code that never emits it is correct.

**Cost, measured.** Cold scan of commons-lang on this machine went 3.4 s /
288 MB to 3.8 s / 337 MB — the extra probes, which the invalidation index
records because it must. The 512 MB ceiling is hard and holds with room;
timing is a target and this spends 13% of it. The tier-1 supertype walk skips
the type's own member types (`file_types` has already answered for those) and
costs nothing at all for a type that names no supertype.

**One writer for the baseline.** `arthron gate --language java --rebase`
records it, since #10 gave the command a language. The `#[ignore]`d test that
used to render the file itself is gone: two writers can disagree, and the one
that wrote the file would not be the one the gate compares against.

---

## 2026-07-27 — Wave 1 capabilities land: alias chains, supertype closure, persisted facets

Three capabilities, one branch, every movement attributed per commit and
every touched baseline re-based under the capability rule. Go held
byte-identical on both corpora throughout, as the rule demands.

| Language | before | after | mechanism |
|---|---|---|---|
| TypeScript | 33.0% | **48.5%** | alias entries: re-export chains resolve; +8400 resolved, `WildcardImport` 9004 → 0, external and local-binding unchanged |
| Java | 66.4% | **67.5%** | supertype closure: `UnindexedSupertype` 710 → 94; facets decide the anonymous-class case (one reference moved *into* the denominator — External was false) |
| Python | 57.4% | **58.1%** | closure + C3: `UnindexedSupertype` 1468 → 1184 |
| Go ×2, JavaScript | — | held exactly | no capability touches them; proven, not assumed |

The alias landing is the single largest improvement in the project's
history — bigger than every other commit's effect on every other corpus
combined — and it moved nothing out of the rate's terms.

**The review round found two highs; both were real and both are pinned by
tests.** A barrel mixing a local star-export with an external one silently
dropped the external contribution and resolved the overlap to the local
name — a wrong `Resolved` edge; every star entry now contributes an
identity and a departing star keeps the name set un-enumerable
(`WildcardImport`). And the Python member walk was preorder DFS where
CPython computes C3: `D(B, C)` resolved `self.m()` to `A.m` where Python
calls `C.m` — replaced with real C3 linearization, `super()` following the
MRO minus its head, verified against the interpreter's own answer.

**First resource data point:** peak RSS under a cold scan — commons-lang
339 MB (66% of the 512 MB hard ceiling), everything else ≤ 126 MB.

*Rejected:* treating Python's unchanged django tally after the alias
landing as "nothing happened" — its façade imports already resolved at the
alias definition, so following chains moved edges onto the definitions
without moving a count; the correctness gain is real and the tally was
never the evidence for it.

---

## 2026-07-27 — Production-readiness ratifications: framework edges, dependency-bounded coverage, 0.0.x releases, perf gates

Four decisions taken up front so the remaining waves run without stopping.

**A framework edge is separately provenanced and never moves a language's
rate.** Frameworks add edges the language honestly cannot see (Django URL
dispatch, Spring injection, component references). Each framework's rules
produce edges tagged with their framework and counted per framework, with
their own baselines when measured. Framework facts may consume the language
graph; they never write into language tallies. *Rejected:* folding framework
resolution into the language rate — a Django plugin would "improve Python",
which is the reclassification-inflation class every prior decision refuses,
and it destroys per-corpus comparability.

**Tier-2 gates on an import-resolution rate, and coverage is bounded by the
parser dependency.** Tier 2 extracts definitions, containers, and imports —
no call references — and its per-language rate is resolved imports over
resolved-plus-unresolved imports, same outcome contract, same ratchet
mechanism. Coverage claim corrected from "27 languages" to **every language
the pinned ast-grep ships a grammar for**: full tier-2 for C++ (C rides its
grammar), C#, Kotlin, Swift, Ruby, PHP, Rust, Scala; best-effort (stock
grammar, generic rules, explicitly non-blocking) for Dart, Elixir, Haskell,
Lua, Bash, and HCL. Erlang, OCaml, Julia, R, Zig, Groovy, Perl and SQL are
out until ast-grep ships them — a pinned-version bump is a deliberate
decision, not drift. *Rejected:* custom tree-sitter grammars for the missing
eight (spend on communities too small to block production readiness);
tier-2 with no rate at all (unfalsifiable "support").

**Releases start at 0.0.1 and publishing is pre-authorized.** First publish
is 0.0.1 to crates.io (the 0.0.0 contract crate already reserves the name);
each completed wave bumps 0.0.x. Publication happens autonomously when all
corpus gates pass, CI is green on Linux, macOS and Windows, docs are
complete, the performance evidence is recorded, and `cargo publish
--dry-run` is clean. GitHub-Packages npm stays a reservation; public npmjs
is skipped. *Rejected:* holding each publish for a manual word (pre-1.0,
yankable, name already public).

**RSS is a gate; time is a target — with numbers.** Measured under a 2 vCPU
`taskset` with `/usr/bin/time -v` on a repository of at least one million
lines: maximum RSS under 512 MB is a hard gate, cold and warm both. Cold
scan at or under 60 seconds per million lines and a warm no-change scan at
or under one second regardless of repository size are targets — missing one
is a finding to explain in evidence, not a failure. *Rejected:* timing as a
hard gate (shared runners flake, and a flaky red gate teaches people to
ignore red).

---

## 2026-07-27 — Tier 1 complete: five languages, six corpus gates, one frozen core

The three language tracks ratified in the road-to-27 plan ran concurrently
over the frozen Phase 2 core and landed the same day. Every rate below is
per-language, measured on a release build against a cold store, and gated by
a committed baseline the CLI itself wrote.

| Language | Corpus | resolved | external | local-binding | unresolved | rate |
|---|---|---|---|---|---|---|
| Go | codeiq | 4467 | 6085 | 4276 | 799 | 84.8% |
| Go | caddy | 3006 | 9571 | 9425 | 1815 | 62.4% |
| Java | commons-lang | 38940 | 68333 | 2062 | 19708 | 66.4% |
| JavaScript | fastify | 2795 | 5159 | 21542 | 1640 | 63.0% |
| TypeScript | vue-core | 17897 | 3694 | 9564 | 36345 | 33.0% |
| Python | django | 18850 | 13326 | 826 | 14017 | 57.4% |

The floors are honest and named: Java's `AmbiguousOverload` 10302 and
`NeedsExpressionType` 6566; TypeScript's `NoMatchingDefinition` 14672 and
`WildcardImport` 9004 (dominated by barrel re-exports the store cannot yet
follow — see the alias gap below); Python's `NeedsTypeInference` 10256.
TypeScript's 33.0% is the honest cost of a monorepo built on `paths`
mappings and barrels; raising it is capability work, not reclassification.

**Going live edits one file.** A track enables itself in its own module
(`scan: None` → `Some`), and the registry treats a disabled track as owning
no files. The three tracks' pull requests touched no shared file and merged
without a single conflict. Two supporting rules earned their keep: a scan
may only declare deleted the files whose extension it owns, and the manifest
fence is per language with an empty digest meaning no opinion.

**The frozen core held, because it was policed.** Three core defects were
found by tracks that refused to work around them in their own code, and each
became a dedicated core PR before any track hit it in anger: registry tests
that asserted Go was alone (every go-live would have broken them); a global
manifest fence (the second live track wiped the first's graph on every
scan); a CLI that could neither scan a Go-less repository nor gate any
language but Go. No track ever edited a core file.

**Every adversarial finding was reproduced or rebutted, none blanket-accepted.**
Java: 11 findings, 11 fixed, 0 rebutted — headline, wrong *resolved* edges
from an erased anonymous-class frame, caught only by new tests that assert
the name a row resolved to, since a wrong edge moves no count. Python: the
review moved the rate 57.2% → 57.4% with the movement itself audited — three
fixes pulled 44 references *out* of `External` back into the denominator
(lowering pressure), one linked 61 references that never needed inference;
net gain is linking, not reclassification. EcmaScript: static and instance
members had collapsed onto one identity; the discriminator now rides the
owner chain as the `prototype` segment, which ES conveniently forbids as a
static member name.

**Named core work for Phase 4.5**, each with its measured cost today:
definition facets are not persisted, so no resolver can branch on a stored
facet; the driver never runs the supertype-closure phase (`link_kinds` is
declared and never called) — `UnindexedSupertype` 710 in Java, 1468 in
Python; the store never surfaces alias entries, so re-export chains stop one
hop short — the bulk of TypeScript's 14672 `NoMatchingDefinition`.

**Rejected:** merging JavaScript and TypeScript into one reported rate (one
resolver family, one domain, two languages — a `.ts` import may name a `.js`
definition, and the two numbers still never aggregate); shrinking any floor
by guessing (every review was instructed to flag decidable cases inside a
floor, and did).

---

## 2026-07-27 — Seven adversarial-review findings, and two reserved characters

An adversarial review of the phase-2 core produced seven findings. Each was
reproduced by a test written before its fix; none was rebutted. Two corrupted
the resolution rate directly, which is why the round is recorded here and not
in commit messages alone.

**A clause header does not bind its own right-hand side.** Go starts a
declared identifier's scope at the end of its declaration, so `if x := x()`
names the outer `x` on the right and the new one only in the body. The
extractor read the whole clause as bound, moving real references into
`LocalBinding` — excluded from *both* terms of the rate, so the bug raised
the rate by deleting edges. The `statement_list` arm beside it already made
the position check; the header arm did not.

**A row key carries the extractor's binding verdict.** A block-local `x()`
and the package-level `x()` after it agree on file, enclosing function, site
text and arity, and resolve differently. One row carries one outcome, so they
merged and both occurrences were attributed to whichever came first — while
the resolved one still inserted its edge, leaving a `Resolved` edge whose row
said `LocalBinding`. Count conservation never noticed: the totals summed, and
only the rate moved.

**`#` separates a container from its members; `!` marks an external test
package.** A Go import path may carry a dot inside a path element
(`gopkg.in/yaml.v3`, or any directory named `p.Foo`), so `{pkg}.{name}` gave
the function `Foo` of package `example.com/m/p` and the package in directory
`p.Foo` one identity and one node — the survivor a `Definition` carrying both
files' declaration sites. `#` is forbidden in an import path and in an
identifier alike, so a definition FQN carries exactly one and a container FQN
carries none. *Rejected:* keeping `#test` for the external test package,
because under the new grammar `{dir}#test` is exactly the FQN of a definition
named `test`, and `func test()` is an ordinary unexported helper. *Rejected:*
`:`, which would erode the invariant that no FQN contains one — the
`external:` prefix rests on it. Two reserved characters, one job each.

**The manifest is a scan input, so the store fences on a fingerprint of it.**
`go.mod` has no extension the language owns and contributes no facts of its
own, yet its module directive roots every FQN. Rewriting it renamed every
node while no `.go` file's bytes moved, so the changed set came out empty and
the store kept a graph no cold scan would build. A resolver now publishes a
digest of what phase 0 read, and a different one wipes the store exactly as a
schema change does. *Rejected:* folding the learned container names into the
digest — the driver teaches those from the store as the scan runs, so the
graph would be wiped on every scan.

**Invalidation compares meaning, not only identity.** A package's node is its
import path, which its directory decides, so rewriting a `package` clause
moves no `NodeId` and still changes what every unaliased import of it binds.
The touched set is now every identity whose payload differs on either side.
Declaration sites stay out of that payload: they move on any edit above them
and nothing resolves against them.

**Both phases decide container identity with one set of names.** Phase 1 saw
only what earlier scans stored and phase 2 what phase 1 had just written, and
whether a file is an external test package is a question about exactly that
difference. A directory whose production package is genuinely named
`api_test` filed its in-package test under one namespace and sourced that
file's edges at another.

**A declaration site carries what its file declared.** Build-exclusive twins
may declare one FQN as different kinds; the record kept the last writer's
answer, and forgetting that file stranded the answer on the survivor. The
record is re-derived from the sites that remain, first in `(file, line)`
order — a function of the surviving set rather than of write order, which is
what makes a warm store agree with a cold one. This is the per-site storage
`merge_node` had already named as the fix and deferred.

**The counts did not move.** Both corpora gate identically to the previous
baseline — `go/codeiq` 84.8% (4467 / 6085 / 4276 / 799), `go/caddy` 62.4%
(3006 / 9571 / 9425 / 1815), every column unchanged. Seven real bugs, and no
triggering shape for any of them in either corpus, so the ratchet is
untouched and the baselines are not re-based. A fix that moves no measurement
is still a fix; a corpus that cannot see it is a gap in the corpus.

Schema generation went to 5: the row key gained a field, declaration sites
gained a payload, and every identity changed.

---

## 2026-07-27 — The gate is a command with an exit code, and a baseline it cannot game

`arthron gate <corpus> --baseline <file> [--db <path>] [--rebase]` scans a
corpus and compares its per-language counts against a committed baseline.
Exit `0` pass, `1` regression, `2` usage or I/O error. Resolution rate is the
primary gate and it now has a mechanism instead of a paragraph.

**The baselines are measured, and here they are.** Release build, cold store,
at the candidate-invalidation commit:

| Corpus | resolved | external | local-binding | unresolved | rate |
|---|---|---|---|---|---|
| `go/codeiq` | 4467 | 6085 | 4276 | 799 | 84.8% |
| `go/caddy` | 3006 | 9571 | 9425 | 1815 | 62.4% |

Both totals conserve: `4467 + 6085 + 4276 + 799 = 15627` extracted references
on the first corpus, `3006 + 9571 + 9425 + 1815 = 23817` on the second — the
same totals as before the binding-environment fix moved references between
categories. `--rebase` refuses to write a baseline whose four counts sum to
zero, because an all-zero file looks exactly as authoritative as a correct one
and every later run would bless it.

**The rate is not stored, only its two terms are.** Comparison is exact
rational arithmetic in `u128` — `now.resolved × was_denom` against
`was.resolved × now_denom` — never floats. At corpus scale a float comparison
is accurate enough; the reason to refuse it is that a stored rate can disagree
with the counts beside it, and then the file no longer says one thing.
*Rejected:* storing the rate as a third number.

**`local_binding` and `external` drift fail the gate unconditionally.** Both
sit outside *both* terms, so moving references into either raises the rate
while deleting real edges — the exact shape of an over-approximating binding
environment passing for a fix. Demonstrated: a baseline claiming a *lower*
rate than the run measured still fails when its `local_binding` differs, so an
"improvement" bought by reclassification cannot land silently. A capability
that legitimately moves `external` re-bases; it never quietly compares.
*Rejected:* a tolerance band on either count — a threshold is a budget, and a
budget gets spent.

**A zero denominator is an error, not a pass.** A rate of zero and the absence
of any reference are different facts, and a gate that called the second one
green would bless a total collapse.

**The baseline format is flat TOML — `key = value`, `#` comments, no tables —
read by a strict reader in this tree.** A table header, an unknown key, a
duplicate key, a missing key, a non-numeric or overflowing count, or a format
version this build does not know is an error, loudly. A baseline that silently
reads as zeros is worse than no baseline.
*Rejected:* `toml` + `serde` for six scalars, in a tree that has neither.

**The default store is a fresh temporary one, deleted after the run.** A warm
store would gate on whatever the previous run happened to leave behind.
`--db` exists to keep the graph for inspection and is documented as such.

**`corpus` and `commit` are provenance: printed, never verified.** A vendored
corpus snapshot carries no git metadata to check them against, so a check
would be theatre. What guards against a baseline recorded from the wrong run
is that the regeneration command is a comment inside the file and the numbers
are quoted here, where a mismatch is visible in review.

---

## 2026-07-27 — Candidate invalidation: an edit reaches the files it changed the answer for

An edit that adds or removes a definition now re-resolves the references in
**unchanged** files that probed that identity. Every reference records the
identities it probed, hits *and* misses, and the misses are the point: a
reference that looked for `pkg.Missing` and found nothing is exactly the one
that must be woken when a later commit declares it.

**Whole affected files are re-resolved, not individual rows.** The index
selects the file; re-resolving one is a parse plus its references through the
per-file replace every changed file already uses.
*Rejected:* patching single rows — it needs sub-file ownership of edges and
candidate entries, which is more machinery, more ways to be subtly wrong, and
no measured need. Its oracle is already built, so it stays available as a
later optimisation.

**One round terminates.** Re-reading a file whose bytes did not move cannot
change what it declares, so the woken set cannot widen the event again.
*Rejected:* a fixed-point loop with an iteration cap — nothing in Go needs one,
and a cap is a silent-truncation risk bought for a language that does not
exist yet.

**The affected set is selected from ownership read before the event writes
anything.** Read it after phase 1 and the comparison is against itself: the
set comes out empty, and every test whose caller happens to sit in the changed
file still passes. It is a deliberate over-approximation — an identity another,
unchanged file also declares is woken though it never disappeared — because
waking too many files is wasted work and waking too few is a wrong answer.

**An edge is a shared fact and now records which files produce it** (store
schema 2 → 3). Two files of one package whose package-level references reach
the same target produce the identical `(src, dst, kind)` triple; deleting one
file used to take the other's edge with it. Nothing in the report notices —
tallies are summed from per-file rows, never from the edge table — so only a
whole-store comparison against a cold scan can see it. Found by exactly that
comparison, at corpus scale, on a file the four-file fixtures could not model:
one `Import` edge between two packages vanished when a file that was not its
only producer was deleted. This is the never-drop rule applied where the key
is not per-file, and it is the same rule a node's declaration sites already
followed.
*Rejected:* re-deriving surviving edges by scanning every file's ownership
record on each delete — correct, and O(repo) per event.

**The oracle is the deliverable, and it runs against both corpora.** After
touch, delete and restore events, the incremental store is compared to a cold
scan of the same tree — snapshot and report, every node, row, edge and
candidate entry — **after each event, not once at the end**, because a delete
followed by a restore puts every identity back and makes a store that went
stale in between look correct again. Both corpora land byte-identical,
including the one holding 28 FQNs that two files each declare.

Counts are unchanged from the binding-environment entry below, which is the
requirement: this stage moves no reference between categories. Release build,
cold store: first corpus `4467 / 6085 / 4276 / 799`, rate 84.8%; second corpus
`3006 / 9571 / 9425 / 1815`, rate 62.4%, 28 FQN collisions.

---

## 2026-07-27 — Go binding environments: the false-edge fix, measured

The shadowing bug the road-to-27 entry described is closed. The extractor now
computes one file-local fact per reference — *some enclosing binder binds the
root of this target* — and the resolver turns it into `LocalBinding`, reported
on its own line beside `External` and outside both terms of the rate.

**The extractor states the fact; the resolver owns the outcome.** A `bool` is
all that crosses the boundary, because every Go binder for a value name is
decidable from one file's AST: parameters, named results and receivers bind
the whole body; block, case and select statements bind from the end of their
declaration to the end of their scope; `if`, `for`, `switch` and `select`
headers bind their clause. `_` declares nothing, `=` in a range clause assigns
rather than declares, and package level is not a binding environment at all.
*Rejected:* suppressing these references in the extractor (a drop, and the one
way to improve a rate without improving anything — it removes a category from
the denominator by deleting it); a `RefTarget::Local` variant (only the *root*
of `x.y.z()` is bound, and the member path is irrelevant to that).

**Measured, both corpora, release build, cold store.** Conservation was
checked first: `resolved + external + local-binding + unresolved` equals the
extracted reference count exactly, so nothing was dropped on the way.

| Corpus | | resolved | external | local-binding | unresolved | rate |
|---|---|---|---|---|---|---|
| Go, 15,627 refs | before | 4467 | 6085 | — | 5075 | 46.8% |
| | after | 4467 | 6085 | 4276 | 799 | 84.8% |
| Go, 23,817 refs | before | 3006 | 9583 | — | 11228 | 21.1% |
| | after | 3006 | 9571 | 9425 | 1815 | 62.4% |

**Every moved reference is accounted for by category.** On the first corpus:
`NoMatchingDefinition` −128 and `NeedsTypeInference` −4148, summing to the
4276 local bindings, with `resolved` and `external` **unchanged**. On the
second: `NeedsTypeInference` −9133, `NoMatchingDefinition` −280 and
`external` −12, summing to 9425, with `resolved` unchanged. The 12 that left
`external` are the wrong edges the fix targets, and each was located in the
source: a `log := …` shadowing `import log`, a parameter `hash hash.Hash`
shadowing `import hash`, a named result `uuid` shadowing the `uuid` package, a
`var time time.Time` shadowing `import time`, and eight more of the same
shape. **Zero resolved references moved on either corpus** — the check that
matters, because an over-wide binder would delete real edges while *raising*
the rate, and the primary gate would reward it.

**The rate rise is not a resolution improvement and must not be read as one.**
It is a denominator that shrank by 4,276 and 9,413 references respectively.
The gate that lands next fails on any `local-binding` drift for exactly this
reason.

**An external test package is `{dir}#test`, not `{dir}_test`.** `#` is
forbidden in a Go module path, so the identity is one no real directory can
claim; a directory literally named `foo_test` beside the external test package
of `foo` previously shared its namespace, and a same-package candidate could
cross between them. Classification also now requires the file to *be* a test
file: `package foo_test` in a non-`_test.go` file is not an external test
package, because the toolchain rejects that directory outright, so the suffix
alone was never the rule.
*Rejected:* a separate `Domain` for Go test packages — a domain is a
language's identity space, and minting one per package flavour would make
every probe between production and test code impossible, which is the opposite
of what an external test package needs, since it imports the package under
test.

**Known and not fixed here:** a file named exactly `_test.go` is ignored by
the Go toolchain and is not ignored by this walk. Fixing it changes the file
set, which changes the denominator, so it belongs to its own commit.

---

## 2026-07-27 — The road to 27 languages: core refactor first, ratified interface, staged corpora, frozen-core fanout

Five language case studies (Java, JavaScript, TypeScript, Python, and a Go
retrospective — design artifacts, kept local) were distilled into a
language-neutral core interface. A grilling session ratified that interface
and fixed the phase order. Four decisions, each with what it displaced.

**Phase 2 is one core-refactor milestone.** The full core interface is
implemented with Go as the only `Language` impl, inside a single window that
takes the one permitted store-schema break and the one permitted contract
break together. It includes the false-edge fix: today a local, parameter, or
receiver that shadows an imported package name produces a **wrong resolved
edge** — verified by executing probe programs, and worse than any miss,
because a miss is counted and a wrong edge is not. The corpus is re-measured
after the refactor with per-commit attribution.
*Rejected:* evolving the trait piecemeal under the later language tracks
(churns the boundary three tracks depend on and multiplies break windows);
hot-fixing the shadowing bug on the old `Reference` type first (builds
local-binding tracking twice).

**Four amendments to earlier decisions, ratified.**

1. *`LocalBinding` leaves the rate.* A reference to a local names a thing
   that is not a node by decision, so the outcome is policy-caused, not a
   language-support failure. It is reported on its own line beside
   `External` and excluded from both terms of the resolution rate. Guard,
   because the exclusion is gameable: the baseline file tracks the
   `LocalBinding` count beside the rate, so reclassification drift is as
   visible as a rate drop.
   *Rejected:* counting it `Unresolved` (punishes the locals-are-not-nodes
   policy and invites "fixing" the number by making locals nodes).
2. *Baselines re-base on capability landings.* A capability that turns
   `Unresolved` into `External` — receiver typing will, at scale — shrinks
   both terms of the rate, which then jumps without one new in-repository
   edge existing. When such a capability lands, the baseline is re-based,
   not compared, and the external count is tracked as a time series. This
   amends the ratchet decision, which assumed the denominator was stable.
   *Rejected:* comparing across a capability landing (rewards
   reclassification as if it were resolution).
3. *The dedup row key gains a discriminator.* `(file, kind, raw_target)`
   merges Java's `f.m(a)` with `f.m(a, b)` and Python's `self.run` across
   two classes in one file — one row, one outcome, two correct answers. The
   key gains an argument count and an enclosing discriminator where the
   language needs one. Schema change; rides Phase 2's break window.
4. *The unresolved-reason taxonomy grows to 18; `MacroGenerated` becomes
   `Generated`.* The rename keeps its wire code; the taxonomy gains the
   reasons the case studies demanded (`LocalBinding`, `NeedsReceiverType`,
   `InterfaceDispatch`, `AmbiguousOverload`, `AmbiguousExport`,
   `DynamicModuleSpecifier`, `ModuleNotFound`, `WildcardImport`, and
   friends). Twelve proposed reasons were rejected in the design artifact.
   Pre-0.1 with no crate consumers is exactly when a rename is cheap.
   *Rejected:* keeping `MacroGenerated` and adding `Generated` beside it
   (carries a dead variant forever).

**Corpora are staged; the second Go corpus arrives now.** `caddy` is
vendored into the corpus repository immediately, so Phase 2's re-measure
gates on two independent Go repositories — the ratchet was already rejected
on one corpus that four of five fixes couldn't move. Tier-1 corpora (one
each for Java, JavaScript, TypeScript, Python) are vendored in Phase 3 when
their `Language` impls start; tier-2 corpora are chosen per-language inside
the Phase 5 fanout.
*Rejected:* vendoring all 27 up front (snapshots go stale before their
language exists, and provenance nobody exercises is decoration).

**Phase 4 runs three concurrent tracks over a frozen core.** Java, JS/TS,
and Python start together when Phase 2 lands. JS and TS are **one track**:
they share module resolution, and identity hashes the *domain*, so a `.ts`
import naming a `.js` definition is one reference space, not a cross-language
edge. Per-language gates are never aggregated, so no track can move another's
number. The core is frozen by default for the duration: a track that finds a
core gap files a spec amendment and a dedicated core PR, which must keep
every already-landed language's baseline green (re-based only under the
capability rule above); no core change ever rides inside a language PR.
*Rejected:* staggering the tracks (serializes without adding safety the
independent gates don't already provide); per-track core edits (reintroduces
exactly the cross-language regression smear that motivated gates-first).

---

## 2026-07-27 — Resolver honesty fixes move the Go baseline

Re-measured after the three fix commits, so the recorded baseline is what the
engine actually does rather than what it did before them. Release build,
`arthron scan corpus/go/codeiq` against a cold store, `codeiq` pinned at
`6dd90b5`, 397 `.go` files.

| | resolved | external | unresolved | `NeedsTypeInference` | `NoMatchingDefinition` | rate |
|---|---|---|---|---|---|---|
| before | 4467 | 6083 | 5077 | 4826 | 251 | 46.80% |
| after | 4467 | **6085** | **5075** | **4824** | 251 | 46.81% |

The rate still prints as 46.8%. That is the honest headline: three correctness
fixes moved two references, and none of them moved the number the gate watches.

**What each fix did, measured one commit at a time** — each commit built and
run in isolation, because attributing a delta to a fix without measuring the
commit that contains it is a guess:

- **Extraction — raw-string imports, multi-name spec definitions:** zero
  references moved. The corpus writes no `` import `fmt` `` and no
  `const A, B = …` whose bogus `,` definition anything went on to name. The
  fixes are real; this corpus does not exercise them.
- **Resolution — version-aware import binding:** the entire delta. Two
  references, both `yaml.Marshal(…)`, in the two files that import
  `gopkg.in/yaml.v3` *without* an alias. Before, the unaliased import bound
  `yaml.v3`, so the qualifier `yaml` was read as a variable and the call was
  charged to `NeedsTypeInference`; now it binds `yaml` and the call is
  correctly `External`. The third `yaml.*` call site was already correct — its
  file writes the alias out (`yaml "gopkg.in/yaml.v3"`), which is precisely
  what made the bug survive a reading of the corpus.
- **Resolution — probe-before-builtin, probed-only candidates,
  whitespace-robust `go.mod`:** zero references moved. The corpus declares no
  package-level shadow of a builtin, and its `go.mod` is space-separated.
- **Package identity — declared package names:** zero references moved. No
  *importable* package in the corpus declares a name its directory does not
  already carry; the only mismatches are two `package main` directories under
  `cmd/`, which nothing may import. (Grepping for `^package` finds three more,
  all inside raw-string Go fixtures in `_test.go` files — the extractor parses
  the real AST and never saw them.)
- **Package identity — `init` is not a node, `_test` is its own package:**
  zero references moved, and node counts changed, which is the expected shape.
  116 `func init()` declarations across 20 packages stopped producing
  definition nodes — as one `{pkg}.init` identity per package, not 116, since
  they collapsed onto each other. 26 external-test files across 6 packages
  moved their definitions into `{pkg}_test`. The predicted risk that `_test`
  namespacing would *lower* `resolved` by breaking a wrong resolution did not
  materialise: no production file in this corpus was resolving to a
  test-only definition.

**Completeness is now asserted, not claimed.** A rate is no evidence that
nothing was dropped — quietly discarding the references it cannot link would
*raise* it. Both acceptance suites now re-extract independently and assert
that `resolved + external + unresolved` equals the reference count exactly:
**15,627** on the corpus, and it was 15,627 before the fixes too. The three
commits reclassified references and neither gained nor lost one.

**Rejected: ratcheting the gate to 46.8% now.** The baseline is honest but the
fixes it reflects are almost untested by this corpus — four of the five moved
nothing here. A ratchet against a single corpus that exercises so little would
lock in a number, not a capability. The gate command comes with a second
corpus.

Cold scan 0.52 s, maximum RSS 17,536 KB, store 2.3 MB; warm re-scan 0.01 s,
8,320 KB, identical tallies. AMD EPYC 9354P, 8 cores — not the 2 vCPU
reference hardware, so these are an upper bound on this box and not a 2 vCPU
claim.

---

## 2026-07-27 — Package identity: declared names, no `init` node, `_test` is its own package

Three corrections to what a Go node *is*, all of them cases where the graph
named something the language does not.

**Declared package names.** An unaliased import binds the imported package's
**declared** name, which lives in that package's source, not in its path.
Directory `utilx` declaring `package util` is imported as
`example.com/app/utilx` and written `util.Parse`. The pipeline — the only
layer that sees every package — now carries an import-path → declared-name map
and corrects the binding for internal imports; `NodeRecord::Package` gained a
`name` field so a scan that touches one file still knows the names of packages
it did not read. External packages keep the path heuristic: their source is
never indexed, so the path is the only evidence there is.

**`init` is not a node.** Go forbids naming `func init()` — it cannot be
called, assigned, or referred to — and a package may declare any number of
them. By the rule that [a node is a thing a reference can
name](#2026-07-26--graph-model-a-node-is-a-thing-a-reference-can-name), it is
not one. It gets no definition node, and calls inside it are sourced at the
package node, exactly like a package-level variable's initialiser. The corpus
has 116 of them.

**External test packages get their own package.** `package foo_test` in the
directory of package `foo` is a second package sharing a directory. Its
definitions now live under `{pkg_path}_test`, with their own package node. It
reaches the package under test through the explicit import it has to write
anyway.

**Why these three together:** each was a distinct way for one identity to
stand for several things, or for a thing nothing can name to become a node.
Sharing a namespace between production and test packages permits a *false*
resolution — a production file's same-package candidate hitting a test-only
definition — which is worse than a miss, because a miss is counted and a wrong
edge is not.

**Rejected:** deriving internal bindings from the import path (the previous
behaviour: silently turns every call through such an import into
`NeedsTypeInference`); having the resolver read the imported package's source
to find its name (extraction is per-file, and the resolver probes identities
rather than parsing); giving each `init` a file-qualified identity such as
`{pkg}.init@file` (a node no reference can name is still not a node, and files
are fields, not identity); dropping `_test.go` files from the scan (loses real
references, and improves the rate by measuring less).

**Storage:** `NodeRecord::Package` is a schema change. The format is pre-1.0
and unversioned, so existing `.redb` files do not decode — delete and rescan.

**Measured:** the `codeiq` corpus rate is unchanged at **46.8%** (resolved
4,467; external 6,085; unresolved 5,075). These are identity fixes, not rate
fixes: no directory in that corpus declares a package name differing from its
own name, and its external test packages call across the boundary only through
imports, so no reference outcome moves. The evidence for each fix is the
integration test that fails without it, not the corpus number.

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
