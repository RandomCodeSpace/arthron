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

Builds or refreshes the graph and prints per-language resolution rates with a
breakdown of every unresolved reason:

```console
$ arthron scan corpus/go/codeiq
go           resolved 7906     external 12210    local-binding 4308     unresolved 799      rate 90.8% (tier 1: call-graph resolution)
             NoMatchingDefinition 123
             NeedsTypeInference 676
```

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
| `2` | Nothing was measured: usage, I/O or the environment. A config that will not parse, a root that is not there, a store another scan is holding open for writing. Safe to retry. |

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

Nineteen live languages. Coverage is *tiered and declared* rather than assumed,
and every rate below is derived from a committed baseline in
[`baselines/`](baselines) — `resolved / (resolved + unresolved)`, measured on a
release build against a pinned corpus snapshot.

### Tier 1 — definitions, references, and call-graph resolution

Five languages: Go, Java, JavaScript, TypeScript, Python. The rate is over call
sites, imports and type uses.

| language | corpus | resolved | unresolved | rate |
|---|---|---:|---:|---:|
| Go | `codeiq` `853efde` | 7,906 | 799 | **90.8%** |
| Go | `caddy` `853efde` | 9,738 | 1,821 | **84.2%** |
| Go | `probes` `synthetic` | 17 | 0 | **100.0%** † |
| Java | `commons-lang` `598dfc1` | 34,217 | 16,279 | **67.8%** |
| Java | `gson` `3ff35d6` | 12,885 | 6,105 | **67.9%** |
| JavaScript | `fastify` `94bcbcc` | 2,795 | 1,640 | **63.0%** |
| JavaScript | `express` `dbac741` | 2,267 | 5,553 | **29.0%** |
| TypeScript | `vue-core` `fa2885d` | 26,297 | 27,945 | **48.5%** |
| TypeScript | `zod` `1fb56a5` | 10,043 | 26,821 | **27.2%** |
| Python | `django` `af67523` | 19,103 | 6,185 | **75.5%** |
| Python | `flask` `22d9247` | 1,185 | 877 | **57.5%** |

Seven of these eleven numbers moved in one deliberate re-base, and none of them
moved because anything was linked better. Two changes landed together: every
tier-1 track now applies the same rule for a reference rooted at a parameter,
local or receiver — it is reported beside `external`, outside **both** terms of
the rate — and the Go track now emits type uses, which the other four already
did. The first takes references *out* of both terms, so it can raise a rate
without linking anything: Python's rose 17 and 28 points that way, and 7,579
django references moved from `NeedsTypeInference` to `local_binding` without one
of them being resolved. Read the rate next to the `local_binding` column in
[`baselines/`](baselines), which is gated for drift exactly so this cannot be
done quietly.

### Tier 2 — definitions, structure and imports; no verified call edges

Fourteen languages. A tier-2 rate is an **import-resolution rate**, and it is
not comparable to a tier-1 rate — different denominators measuring different
things.

| language | corpus | resolved | unresolved | rate |
|---|---|---:|---:|---:|
| Rust | `ripgrep` `e89fff8` | 649 | 13 | **98.0%** |
| Elixir | `plug` `9fa11c8` | 116 | 1 | **99.1%** |
| Kotlin | `okio` `6604edb` | 683 | 80 | **89.5%** |
| C++ | `fmt` `1be298e` | 127 | 18 | **87.6%** |
| Ruby | `rack` `e1f22fd` | 291 | 50 | **85.3%** |
| PHP | `guzzle` `3aeea04` | 360 | 170 | **67.9%** |
| C# | `serilog` `6d9fc0b` | 53 | 0 | **100.0%** |
| Dart | `collection` `dec28c1` | 75 | 0 | **100.0%** |
| Haskell | `aeson` `e00ef15` | 278 | 0 | **100.0%** |
| HCL | `terraform-aws-vpc` `3ffbd46` | 23 | 0 | **100.0%** |
| Swift | `alamofire` `7595cbc` | 40 | 0 | **100.0%** |
| Scala | `upickle` `87e0b24` | 267 | 364 | **42.3%** |
| Lua | `busted` `56e6d68` | 99 | 153 | **39.3%** |
| Bash | `bats-core` `eb7f42f` | 0 | 6 | **0.0%** |

Tier 2 is honest rather than degraded: you get symbols, structure and imports,
you do not get call edges, and the tool says which is which instead of inventing
the difference. Six tier-2 tracks declare themselves **best effort** in their
own module docs — Bash, Dart, Elixir, Haskell, HCL and Lua — meaning the track
reads a deliberately narrow slice of that language's reference surface, so the
denominator is small by design and the definition census beside it carries most
of the weight. Best effort constrains how much of a language is read; it does
not make the number optional. All six are gated in CI on the same terms as the
other nineteen baselines, and a regression in any of them fails the build.

† `probes` is a synthetic corpus, written to pin resolver behaviour rather than
sampled from a real project. It is listed because it is gated like the others,
not as evidence about real Go.

## How to read these numbers

**A rate is never aggregated.** There is no "arthron resolves N% of code"
figure, and there will not be one. Rates are reported per language, per corpus,
because averaging Go's call-graph rate with Bash's import rate produces a number
that means nothing and hides every regression underneath it. Nineteen languages,
twenty-five committed baselines, twenty-five separate numbers.

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
| Peak RSS | **< 512 MB (hard)** | 337.1 MiB cold-scanning kubernetes v1.36.3, 1,789,247 lines — 66% of the ceiling, six runs spanning 0.2% |
| Cold index throughput | < 60 s / 1M lines | ~17 s / 1M lines on the same scan |
| Warm re-index, unchanged tree | < 1 s | 2.75 s on kubernetes **on the pre-Wave-3 build** — a miss, recorded as a finding rather than hidden, and not yet re-measured |

The first two rows are the shipped build. The third is not, and the column
says so rather than letting three numbers read as one benchmark run: warm
timing was last measured on the binary whose cold RSS was 729 MiB, and the
memory work that replaced it was explicitly not aimed at the warm path (warm
cost is per-file re-read and re-hash). Warm RSS improved 13% on the same
change; warm wall time has not been re-run on the reference hardware, so the
number here is the last one that was actually executed and it is labelled
rather than refreshed by inference.

The RSS number is the interesting one: it was **729.0 MiB** and failed the hard
gate. Bounding it — per-500-file commits on every phase, a capped redb page
cache, phase 2 consuming facts per file — brought it to 337.1 MiB, and
byte-identity of the resulting graph was proven at the level of full blake3
snapshot digests across five corpora, not at the level of matching tallies.

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
