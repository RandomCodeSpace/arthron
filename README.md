# arthron

**ἄρθρον** — *joint*. In anatomy, the articulation where two separate bones meet
and move as one. In Greek grammar, the *article*: the small word whose only job
is binding a reference to its referent.

Local-first code intelligence. Each file is parsed in isolation; a single
**resolver** links the references between files into a verified graph — and
records what it could not link, and why, instead of dropping it.

## Why it is built this way

The usual way to build a code graph is to let a per-file analyzer emit edges. A
file-local analyzer cannot know whether the symbol it just saw is defined
somewhere else in the repository, so it either guesses — and something
downstream discards the guess — or it gives up and emits nothing. Both paths
produce the same artefact: plenty of nodes, almost no edges between files, and a
tool that reports success throughout. You cannot ask it what a change breaks,
because it never linked anything.

arthron inverts the responsibility. **Extractors are forbidden from emitting
edges.** They emit *references* — "this call site names `parseConfig`, in this
scope, at this byte range." One resolver, which sees every file, owns all
linking, and every reference lands in exactly one outcome:

| Outcome | Meaning |
|---|---|
| `Resolved` | Linked to a definition in this repository. Verified. |
| `External` | Linked to a dependency outside it. |
| `Unresolved` | Could not be linked — **recorded with a reason, never dropped.** |

There is no fourth outcome and no way to express "dropped".

## Install

```bash
cargo install arthron
```

Requires Rust 1.89 or newer. One binary, no runtime dependencies. (The `0.0.0`
crate was a name reservation and contains no engine; `0.0.1` is the first real
release.)

## Usage

```bash
arthron scan ./my-repo
```

Builds or refreshes the graph and prints per-language resolution rates, the
share of the language's references the rate is taken over, and a breakdown of
every unresolved reason:

```console
$ arthron scan corpus/go/codeiq
go           resolved 9794     external 12595    local-binding 9873     unresolved 4295     rate 69.5% (tier 1: call, import and type-use resolution)
             rate denominator 14089 of 36557 references (38.5%)
             NeedsTypeInference 770
             NeedsReceiverType 3116
             NeedsExpressionType 409
```

The second line is the one to read next to the rate. `external` and
`local_binding` sit outside both of the rate's terms by design, and on a real
corpus they are most of the rows — so 69.5% here is a rate over 38.5% of the
references Go emitted, not over all of them. Both numbers are true and neither
stands in for the other.

The graph is stored at `<PATH>/.arthron/graph.redb` unless `--db` says
otherwise. Re-running against an unchanged tree re-reads the stored graph: the
changed set is exactly the files whose content hash moved. Cold indexing is the
same code path with a changed set of everything, so there is no separate
incremental mode that can silently skip work.

```bash
cd ./my-repo                              # or pass --db ./my-repo/.arthron/graph.redb
arthron query def    'crypto#Verify'
arthron query refs   'crypto#Verify'
arthron query impact 'crypto#Verify' --depth 3
```

The definition and its declaring sites; every stored reference row that resolved
to it; and what transitively reaches it, layer by layer — the blast radius of a
change. A name may be a full FQN or any suffix of one that starts at a
separator; a suffix several nodes end is answered with *all* of them and exit
code 1, because picking one would be a guess. The store is opened read-only: a
query never creates or rebuilds it — a query is not a scan, so a missing store
is an error rather than a rebuild.

Two things to get right, both of which the shell will tell you about
immediately. A query reads `.arthron/graph.redb` **relative to the working
directory**, not to whatever path was last scanned, so either `cd` into the
repository or point `--db` at its store. And the separator inside an FQN is the
language's own: Go's package/symbol boundary is `#`, so this definition's full
name is `example.com/app/pkg/crypto#Verify` and `crypto#Verify` is a suffix of
it that starts at a separator. `crypto.Verify` is not, and matches nothing.

```bash
arthron gate corpus/go/codeiq --language go --baseline baselines/go-codeiq.toml
```

Scans a corpus and compares its counts against a committed baseline — this is
what makes a resolution-rate regression fail a build. `--rebase` overwrites the
baseline with what the run measured, which is how the ratchet moves up: by a
deliberate commit, never by a number quietly drifting.

### Exit codes

Three, meaning the same three things on every command, because the number is
what a build script reads.

| Code | Meaning |
| --- | --- |
| `0` | The command ran and this is the answer. |
| `1` | The command ran and the answer is **no**: a gate regression, a query that matched nothing or matched several. Never an error — never retry it. |
| `2` | No verdict. Usually nothing was measured: usage, I/O or the environment — a config that will not parse, a root that is not there, a store another scan is holding open for writing — and those are safe to retry. `gate` also answers `2` when the comparison could not be made at all: a baseline or a run whose `resolved + unresolved` is zero has no rate on that side, so the result is neither a pass nor a regression. That case is deterministic; retrying returns `2` again. |

`scan` has no verdict to fail, so `scan` never returns `1`; every failure it can
have is a `2`. A store somebody else is scanning against is the case that
matters in CI: it is a `2`, not the `1` a regression uses, so a build can wait
and try again without ever masking a real one.

`scan`, `gate` and `query` each take `--json` and print one document instead
of the report.

```bash
arthron mcp
```

Serves the graph to an agent over the Model Context Protocol on stdio — JSON-RPC
2.0, one message per line, `scan_repo` / `query_def` / `query_refs` /
`query_impact`. Each returns the same document `--json` prints, from the same
library calls the CLI makes: there is no second answer for agents. No socket is
opened and no address is bound. See [`docs/mcp.md`](docs/mcp.md).

### `arthron.toml`

Optional, at the repository root. Every key is optional too, so a repository
with no config behaves exactly as it would without one.

```toml
include = ["src/**"]        # a whitelist: with any include, an unmatched file is not read
exclude = ["**/vendor/**"]  # wins over include, last-match-wins like .gitignore
db = ".arthron/graph.redb"  # where the graph goes, relative to this repository

[tracks]
java = false                # switch a live track off for this repository
```

The `db` key must stay **inside** the repository: an absolute path, a `..`
that climbs past the root, or a parent directory that is a symlink out of the
tree is refused with exit 2 and nothing is scanned. A scan reads repositories
you did not write, and `db` says where a scan *writes* — so the repository does
not get to choose which of your files a scan replaces. The command-line `--db`
flag is deliberately free to name anything, inside the tree or out: you typing
a path is you saying it, and a file sitting in a scanned tree is not.

The two are also resolved against different directories, and the difference is
worth knowing before it surprises you. The config's `db` is relative to the
**repository it sits in**; `--db` is relative to your **current working
directory**. So `arthron scan ./repo --db graph.redb` writes `./graph.redb`,
not `./repo/graph.redb`. That follows from what each one is — the file is the
repository speaking about itself, the flag is you speaking about your machine
— and it is why the file may not leave its own tree and the flag may go
anywhere. `query` and `mcp` have no such split: they take no repository
argument, so both their config and their `--db` are read relative to the
working directory.

The `[tracks]` keys are track names, not language names, and the two differ in
one place: **`ecma` is the single track that owns JavaScript and TypeScript**,
so `ecma = false` switches off both and neither `javascript` nor `typescript`
is a key. The full set is `go`, `java`, `ecma`, `python`, `cpp`, `csharp`,
`kotlin`, `swift`, `ruby`, `php`, `rust`, `scala`, `dart`, `elixir`,
`haskell`, `lua`, `bash`, `hcl` — eighteen tracks over nineteen languages.

An unrecognised key is refused **by name** rather than ignored — at the top
level and under `[tracks]` — because a silent typo means scanning a different
tree than you believe you are scanning while reporting a number you then trust.
`[tracks]` switches off, never on: a track this build does not implement cannot
be conjured by config.

## Languages

Nineteen live languages. Coverage is *tiered and declared* rather than assumed.
Every column below is read out of a committed baseline in
[`baselines/`](baselines), measured on a release build against a pinned corpus
snapshot; the two derived columns are `rate = resolved / (resolved +
unresolved)` and `rate denom. = (resolved + unresolved) / (resolved + external
+ local_binding + unresolved)`, the share of the language's references the rate
is taken over.

### Tier 1 — definitions, references, and cross-file import and function-call resolution

Five languages: Go, Java, JavaScript, TypeScript, Python. The rate is over call
sites, imports and type uses — the reference kinds these tracks emit.

That is not a complete call graph, and this section used to say it was. Method
dispatch mostly does not resolve yet. A call through a receiver whose type the
enclosing signature states does resolve — Go joined the other four in doing so
— but a call on a value whose type has to be inferred does not, and the stored
reasons are where that shows. The buckets that need a type environment —
`NeedsExpressionType`, `NeedsReceiverType`, `NeedsTypeInference`,
`AmbiguousOverload`, `UnindexedSupertype`, `DynamicDispatch` — are the majority
of what tier 1 leaves unlinked on nine of the ten real corpora, from 51.9% on
`flask` to 100.0% on both Go corpora. The tenth is `vue-core` at 42.1%, where
the largest single bucket is instead the 14,618 names a declared test runner
injects into the global scope.

No one reason stands for that gap, and an earlier draft of this section said
one did — that `NeedsTypeInference` was "the great majority" in all five
languages. It is not, and was not: which bucket leads differs by language and
by corpus — `NeedsReceiverType` on both Go corpora (72.5% and 64.9%),
`AmbiguousOverload` on commons-lang (56.6%), `NeedsExpressionType` on gson,
express, fastify and zod, `NeedsTypeInference` on django (43.3%) — and on
`gson` `NeedsTypeInference` is 72 rows of 6,105. What the leading buckets share
is the type environment none of them has, and that is the work that has not
been done. What tier 1 delivers today is definitions, references, and
cross-file import and function-call resolution with the accounting to show
which is which.

The last column is the rate's denominator as a share of every reference the
language emitted: `(resolved + unresolved) / (resolved + external +
local_binding + unresolved)`. `external` and `local_binding` are legitimately
outside both of the rate's terms, and they are also most of the rows on a real
corpus — Go's 69.5% is a rate over 38.5% of what Go emitted. Publishing the
rate without the share invites reading the first number as the second.

<!-- tier-1 table: every column is read straight out of baselines/<file>.toml.
     rate  = resolved / (resolved + unresolved)
     denom = (resolved + unresolved) / (resolved + external + local_binding + unresolved)
     Refresh = re-read the baselines and re-render these rows; nothing here is
     derived from anything else, and no row may be edited by hand.
     `every_readme_table_row_matches_its_baseline` in tests/baselines.rs
     re-derives all eight cells and fails on any drift, corpus or not. -->
| language | corpus | resolved | external | local-binding | unresolved | rate | rate denom. |
|---|---|---:|---:|---:|---:|---:|---:|
| Go | `codeiq` `853efde` | 9,794 | 12,595 | 9,873 | 4,295 | **69.5%** | 38.5% |
| Go | `caddy` `853efde` | 10,585 | 21,304 | 13,181 | 9,014 | **54.0%** | 36.2% |
| Go | `probes` `synthetic` | 17 | 26 | 1 | 0 | **100.0%** † | 38.6% |
| Java | `commons-lang` `598dfc1` | 34,217 | 63,385 | 15,162 | 16,279 | **67.8%** | 39.1% |
| Java | `gson` `3ff35d6` | 12,885 | 16,737 | 6,706 | 6,105 | **67.9%** | 44.8% |
| Java | `probes` `e4dc880` | 13 | 7 | 1 | 1 | **92.9%** † | 63.6% |
| JavaScript | `fastify` `94bcbcc` | 2,795 | 5,159 | 21,542 | 1,640 | **63.0%** | 14.2% |
| JavaScript | `express` `dbac741` | 2,267 | 702 | 3,039 | 5,552 | **29.0%** | 67.6% |
| JavaScript | `probes` `e4dc880` | 6 | 0 | 1 | 2 | **75.0%** † | 88.9% |
| TypeScript | `vue-core` `fa2885d` | 26,297 | 3,694 | 9,564 | 27,945 | **48.5%** | 80.4% |
| TypeScript | `zod` `1fb56a5` | 17,080 | 1,952 | 8,143 | 19,784 | **46.3%** | 78.5% |
| TypeScript | `probes` `e4dc880` | 12 | 0 | 1 | 3 | **80.0%** † | 93.8% |
| Python | `django` `af67523` | 19,103 | 13,326 | 8,405 | 6,185 | **75.5%** | 53.8% |
| Python | `flask` `22d9247` | 1,185 | 2,317 | 2,146 | 877 | **57.5%** | 31.6% |
| Python | `probes` `e4dc880` | 5 | 0 | 2 | 1 | **83.3%** † | 75.0% |

Ten real corpora and five synthetic probe pins. Since the last published table
four rows are new, three rates moved, one row changed without its rate moving,
and every other cell is byte-identical. A rate moves for more than one reason
and only one of those reasons is "it got better", so each movement below is
attributed per row in [`CHANGELOG.md`](CHANGELOG.md) — by a whole-row join
against a binary built from the previous commit — rather than inferred from
these totals.

1. **Both Go rates fell, and that is coverage.** The extractor now emits the
   two member sites it never had: a selector *read* (`pkg.Name`, `t.field`)
   and a struct literal's field keys. That is 11,334 new occurrences on
   `codeiq` and 13,723 on `caddy`, of which 1,778 and 377 resolve; most of the
   rest are `NeedsReceiverType`, because a Go struct field is not a node in
   this build. The rate's denominator grew 8,900 → 14,089 and 12,908 → 19,599,
   the rate fell 90.1% → 69.5% and 79.1% → 54.0%, and **nothing that resolved
   before stopped resolving**. Two rates taken over different reference sets
   are not two measurements of the same thing.
2. **`zod` rose 19.1 points because one config key is now read.**
   `compilerOptions.customConditions` points zod's self-imports at its own
   sources instead of at a built `.d.cts` that no scan of the sources can see:
   `ModuleNotFound` 7,822 → 1, resolved 10,043 → 17,080, and every one of the
   7,037 new edges lands in `packages/zod/src/`. That is linking that was
   previously missed, not reclassification.
3. **A rate can rise with nothing linked at all, and one did.** A reference
   whose *root* is a parameter or a local is reported beside `external`,
   outside **both** terms of the rate, however long the member path after it —
   so unifying that rule across the five raised Python's two rates by 17 and 28
   points while 7,579 django references moved from `NeedsTypeInference` to
   `local_binding` without one of them being resolved. Read the rate next to
   the `local_binding` column in [`baselines/`](baselines), which is gated for
   drift exactly so this cannot be done quietly.

`express` is the row that changed without its rate moving: `XMLHttpRequest` was
missing from the host global list while `WebSocket`, `AbortController` and
`fetch` were all in it, so `external` went 701 → 702 and the rate reads 28.99%
on both sides. The same change reclassified the 1,728 express and 13,833
vue-core references that a declared test runner injects — `it`, `describe`,
`expect` — out of `NoMatchingDefinition`, a bucket whose contract is that it
means *arthron's* bug. They became `Unresolved(UnknownPackage)` rather than
`External` deliberately: `Unresolved` keeps them inside both of the rate's
terms, where `External` would take them out of both and raise the rate without
linking anything. No gated number moved.

What the third change costs is *edges*, and the counts are in
[`CHANGELOG.md`](CHANGELOG.md): 5,374 commons-lang and 3,189 gson occurrences
that `arthron query` and the MCP server used to answer with a definition are
reported on the `local_binding` line instead — 13.6% and 19.8% of those
corpora's resolved edges. The reason claims the reference is not evidence about
cross-file linking, because reaching its target needs the type of a binding no
other file can see. It does not claim the target has no name.

### Tier 2 — definitions, structure and imports; no verified call edges

Fourteen languages. A tier-2 rate is an **import-resolution rate**, and it is
not comparable to a tier-1 rate — different denominators measuring different
things. The `rate denom.` column is the same share as tier 1's, and it is where
that difference shows: `local_binding` is zero across all fourteen, because a
track that emits no expression-level reference has no local to bind.

<!-- tier-2 table: every column is read straight out of baselines/<file>.toml.
     rate  = resolved / (resolved + unresolved)
     denom = (resolved + unresolved) / (resolved + external + local_binding + unresolved)
     Refresh = re-read the baselines and re-render these rows; nothing here is
     derived from anything else, and no row may be edited by hand.
     `every_readme_table_row_matches_its_baseline` in tests/baselines.rs
     re-derives all eight cells and fails on any drift, corpus or not. -->
| language | corpus | resolved | external | local-binding | unresolved | rate | rate denom. |
|---|---|---:|---:|---:|---:|---:|---:|
| Rust | `ripgrep` `e89fff8` | 649 | 411 | 0 | 13 | **98.0%** | 61.7% |
| Elixir | `plug` `9fa11c8` | 116 | 55 | 0 | 1 | **99.1%** | 68.0% |
| Kotlin | `okio` `6604edb` | 683 | 1,136 | 0 | 80 | **89.5%** | 40.2% |
| C++ | `fmt` `1be298e` | 127 | 254 | 0 | 18 | **87.6%** | 36.3% |
| Ruby | `rack` `e1f22fd` | 291 | 1 | 0 | 50 | **85.3%** | 99.7% |
| PHP | `guzzle` `3aeea04` | 360 | 265 | 0 | 170 | **67.9%** | 66.7% |
| C# | `serilog` `6d9fc0b` | 53 | 36 | 0 | 0 | **100.0%** | 59.6% |
| Dart | `collection` `dec28c1` | 75 | 49 | 0 | 0 | **100.0%** | 60.5% |
| Haskell | `aeson` `e00ef15` | 278 | 796 | 0 | 0 | **100.0%** | 25.9% |
| HCL | `terraform-aws-vpc` `3ffbd46` | 23 | 1 | 0 | 0 | **100.0%** | 95.8% |
| Swift | `alamofire` `7595cbc` | 40 | 130 | 0 | 0 | **100.0%** | 23.5% |
| Scala | `upickle` `87e0b24` | 267 | 0 | 0 | 364 | **42.3%** | 100.0% |
| Lua | `busted` `56e6d68` | 99 | 0 | 0 | 153 | **39.3%** | 100.0% |
| Bash | `bats-core` `eb7f42f` | 0 | 0 | 0 | 6 | **0.0%** | 100.0% |

Tier 2 is honest rather than degraded: you get symbols, structure and imports,
you do not get call edges, and the tool says which is which instead of inventing
the difference. Six tier-2 tracks declare themselves **best effort** in their
own module docs — Bash, Dart, Elixir, Haskell, HCL and Lua — meaning the track
reads a deliberately narrow slice of that language's reference surface, so the
denominator is small by design and the definition census beside it carries most
of the weight. Best effort constrains how much of a language is read; it does
not make the number optional. All six are gated in CI on the same terms as the
other twenty-three baselines, and a regression in any of them fails the build.

† `probes` is a synthetic corpus — one per tier-1 language, hand-written to pin
resolver behaviour name by name rather than sampled from a real project. The
five are listed because they are gated like the others, not as evidence about
real Go, Java, JavaScript, TypeScript or Python: a hand-written rate is a
property of the fixture, so these five are **pins, not ratchets**, and must
never be re-based to claim a capability. They pin the misses as exactly as the
hits, including two known *mislabels* — `this.greet` across a module boundary
reports `UnindexedSupertype` when no conjunct of that reason holds, in both
ECMAScript dialects — so that the day either is fixed, the named rows move and
the probe says which. `arthron` holds the facts to resolve that row today; the
fix is a re-base of two pins and a `docs/decisions.md` entry, and it moves
nothing on express, fastify, vue-core or zod.

## How to read these numbers

**A rate is never aggregated.** There is no "arthron resolves N% of code"
figure, and there will not be one. Rates are reported per language, per corpus,
because averaging Go's tier-1 rate with Bash's import rate produces a number
that means nothing and hides every regression underneath it. Nineteen languages,
twenty-nine committed baselines, twenty-nine separate numbers.

**`Unresolved` is data, not failure.** Every unresolved reference is stored with
a reason — `NeedsTypeInference`, `NoMatchingDefinition`, `ModuleNotFound`,
`AliasCycle`, `DynamicDispatch` and the rest. Aggregated, those reasons say
exactly where language support is thin, which is what makes the next
improvement a decision instead of a guess. A resolver that dropped its hardest
references would report a *better* rate for doing less work; that is the failure
mode the three-outcome contract exists to make impossible.

**`External` and `LocalBinding` sit outside both terms of the rate.** A
dependency import is not a resolver failure, and a reference to a parameter or
local variable is excluded by policy — locals are not nodes. Neither may be used
to inflate a rate, so the gate checks both for drift and fails on any movement
in either.

**The rate's denominator is published beside it, every time.** Excluding those
two is correct and it also makes the denominator smaller than the surface the
scan read — on `fastify` the rate covers 14.2% of the references JavaScript
emitted. A high rate over a small share is a real measurement and a partial
one, and the only way to read it as the first without the second is for the
second not to be there. So `arthron scan` prints it under every language line,
and every table here carries it as a column. It is not in `--json`: the
document already has all four counts, and a consumer divides.

**The gate refuses a shrinking denominator.** At a 100% baseline a *dropped*
`Resolved` row would otherwise be invisible, and at any baseline a dropped
`Unresolved` row reads as an improvement. A run whose `resolved + unresolved`
falls below the baseline's fails with `denominator_shrank`; growth stays legal.

**Bash sits at 0.0% over a denominator of 6, and that is a working gate.**
bats-core was vendored *because* not one of its `source` targets is a literal
path — they are composed at run time out of shell variables. Matching on the
tail of a composed path would take the number from 0% to 50% in one commit and
would be a guess about values the running program computes. A small honest
denominator with the right reason on every miss is the deliverable; the drift
checks on `external` and `local_binding` — both zero — are what keep it
un-gameable, and the 91-function definition census beside it is what the track
is actually for.

## Reference hardware

**2 vCPU** — a CI runner, not a workstation. arthron runs inside CI jobs and
agent loops, where being predictable matters more than being fast on a
developer's laptop. Resource ceilings are hard limits; timings are targets.

| Budget | Target | Measured |
|---|---|---|
| Peak RSS | **< 512 MB (hard)** | 280.2 MiB cold-scanning a 5,353,211-line Go tree yielding 1,678,021 references — 54.7% of the ceiling, worst of five runs spanning 468 kB |
| Cold index throughput | < 60 s / 1M lines | 20.5 s / 1M lines on the same scan, five runs spanning 19.9–20.5 — but **70.1 s / 1M on TypeScript**, a miss, measured below |
| Warm re-index, unchanged tree | < 1 s | 12.37 s re-scanning that same tree, unchanged — a miss by 12×, recorded as a finding rather than hidden |

The RSS percentage and the ceiling are read in the same binary units: 280.2 of
512 is 54.7%. That is the basis every RSS verdict on this project was recorded
on — the 729.1 MiB that failed this gate once was recorded as **1.42× the
ceiling**, which is 729.1/512 — so it is said here rather than left to be
inferred. Read `512 MB` as decimal MB instead and the same measurement is
293.8 MB, 57% of it: under the ceiling on either reading, but only one of the
two matches the record.

All three rows are now the shipped build on one tree, cold-scanned and then
re-scanned unchanged: 5,353,211 lines, where the first two rows used to name
1,789,247 and the third a build two waves older. The earlier 337.1 MiB is
therefore not restated beside them — it was a real measurement, of a smaller
tree, by a build that no longer exists here, and setting it next to a number
from neither would invite the two to be read as a before-and-after of the same
scan.

Warm is the miss, and it is not this build's doing: 11.73 s and 12.37 s over
two runs here, against 12.35 s and 12.63 s for the same pair on the commit
before the memory change, with warm peak RSS identical to within 0.2%
(125,872–126,012 kB against 125,880–126,008 kB). Warm cost is per-file re-read
and re-hash of every file in the tree, so it tracks the tree's size rather than
the changed set: a target set against a 1.8M-line tree is missed by 13× against
a 5.35M-line one. It is stated as measured rather than normalised away.

Cold throughput is a per-language number, and one language misses the target.
Median of three runs each on the shipped build, per 1M lines: Go `caddy` 32.6 s,
Java `commons-lang` 44.4 s, Java `gson` 48.7 s, JavaScript `fastify` 37.1 s,
JavaScript `express` 42.9 s, Python `django` 42.7 s, Python `flask` 43.5 s, and
the 5.35M-line Go tree 20.2 s — every one inside the 60 s target — against
TypeScript `vue-core` at **70.1 s** and TypeScript `zod` at **82.5 s**, both
outside it. The extra parse per file that bought the memory ceiling roughly
doubled all of them, and TypeScript was the language with the least room:
36.7 s and 45.3 s before it. Timing is a target and the ceiling is hard, so
this ships — but a target that is now missed is recorded here rather than left
to be inferred from the Go row.

The RSS number is the interesting one, and it has failed this gate twice. It
was **729.1 MiB**; per-500-file commits on every phase, a capped redb page cache
and phase 2 consuming facts per file brought it to 337.1 MiB. Then Go learned to
emit type uses, non-call selector reads and composite-literal keys, references
per tree went up 2.82×, and a cold scan reached **158.4% of the ceiling** —
because the walk's references were held until phase 2 consumed them, 89.8% of
the peak. The walk now keeps a file's declarations and forgets its references,
and each later phase reads the file again. Both times byte-identity of the
resulting graph was proven before the change shipped: today that is all 29
corpus gates and all 15 target-pin comparisons producing byte-identical output,
not merely matching tallies. See [`docs/decisions.md`](docs/decisions.md).

## No network calls, ever

Not for dependency metadata, not for telemetry, not for updates. `arthron mcp`
speaks stdio and binds no socket. Nothing about your code leaves the machine.

## Contributing

Decisions and the reasoning behind them are in
[`docs/decisions.md`](docs/decisions.md), newest first, including what was
rejected. Read the relevant entry before changing something it covers; if you
change it anyway, add an entry saying why. [`CONTEXT.md`](CONTEXT.md) is the
glossary — use its terms, respect its `_Avoid_` lists.

Measurements must be executed, never estimated. A benchmark that turns out to be
invalid stays in the record with the reason it was wrong.

The corpora are not vendored here. They live in the private
`RandomCodeSpace/arthron-corpus`; clone it into `corpus/` (gitignored) to run
the acceptance tests, which skip when it is absent.

## License

MIT. See [LICENSE](LICENSE).
