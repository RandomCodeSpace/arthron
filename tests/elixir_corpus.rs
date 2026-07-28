//! Acceptance for the Elixir track against the plug corpus: nothing is
//! dropped, the tier-2 contract holds on real code, and the measured counts
//! are the ones the committed baseline was recorded from.
//!
//! Elixir is a **tier-2, best-effort** language here, so what this file gates
//! is an **import-resolution rate** — `Resolved / (Resolved + Unresolved)`
//! over the `alias`, `import`, `require` and `use` directives the extractor
//! emits, one reference per module named, and nothing else. It is not
//! comparable with Go's or Java's rate, and it is never aggregated with
//! either.
//!
//! Five questions, because a rate is only worth reading if you can answer all
//! of them:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** Tier 2's deliverable is definitions, structure and
//!    imports, and the rate can only see the imports. The definition census
//!    is therefore asserted exactly on both sides of the store, by kind and
//!    by name — an owner-frame bug that lost most of the corpus's functions
//!    moves no rate, no bucket and no baseline, so nothing else here would
//!    notice it.
//! 3. **The composed names.** 59 of the corpus's 142 modules are declared
//!    under a name their own source never writes, because a nested
//!    `defmodule` composes through the one that encloses it. Those are
//!    counted, and four of them are pinned by name with their declaration
//!    lines — including the one the corpus provenance calls out.
//! 4. **The external set, by name.** `External` sits outside both terms of
//!    the rate, so an in-repository module filed into it disappears from the
//!    measurement instead of failing it. Every external module is listed
//!    here, so a composition bug shows up as an *addition* to that list
//!    rather than as a quietly better rate.
//! 5. **The ratchet.** The counts are compared against
//!    `baselines/elixir-plug.toml` through the same
//!    [`arthron::gate::evaluate`] the `arthron gate` command uses, so a rate
//!    regression — or drift in either of the two buckets that sit outside the
//!    rate — fails the build.
//!
//! plug is pinned and is never edited, so every number below is a fact about
//! this extractor and this resolver reading a fixed 76 files; a change to any
//! of them is a change in what the track *does*, and must arrive as a
//! deliberate edit here and a deliberate `--rebase` beside it, never as a
//! test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/elixir/plug --language elixir \
//!     --baseline baselines/elixir-plug.toml --rebase --commit 9fa11c8
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken track.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Domain, Lang, RefKind, node_id, reason_name};
use arthron::pipeline::source_files;
use arthron::query::{NodeKind, definition};
use arthron::resolution_rate;
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_elixir::extract::extract;
use arthron::track_elixir::lang::ElixirLang;
use arthron::track_elixir::resolve::scan_elixir;

const CORPUS: &str = "corpus/elixir/plug";
const BASELINE: &str = "baselines/elixir-plug.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
const FILES: usize = 76;

/// Directive references, one per module named.
///
/// **172, where a line-oriented reading of the same tree counts 199.** The
/// 27-site difference is entirely documentation: `lib/plug/error_handler.ex`
/// writes `use Plug.Router` and `use Plug.ErrorHandler` inside its
/// `@moduledoc` heredoc and imports nothing at all, and
/// `test/plug/error_handler_test.exs` writes two more inside a
/// `Code.eval_string("""…""")`. A parser does not read a string as source,
/// and that difference is asserted by name below rather than left as a
/// discrepancy between this file and the corpus provenance.
const REFERENCES: u64 = 172;

/// Directive references by `(directive, form)`.
///
/// The `require dynamic` entry is the corpus's one un-nameable target:
/// `require unquote(target)` in `lib/plug/router.ex`, inside a `quote` block.
const FORMS: &[(&str, u64)] = &[
    ("alias module", 24),
    ("import module", 65),
    ("require dynamic", 1),
    ("require module", 11),
    ("use module", 71),
];

/// Every definition the extractor emits over those 76 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. Definitions
/// are the half of tier 2 the import-rate gate cannot see: a frame bug that
/// lost most of the functions in the corpus would leave every rate, every
/// bucket and the whole ratchet untouched.
///
/// `Module` counts `defmodule`, `defprotocol` and `defimpl` alike — Elixir
/// files all three in one namespace, because all three declare a module.
/// `Field` is one per `defstruct`/`defexception` key. `Function` counts every
/// clause separately; the store's census below is where they become one node
/// each.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 969),
    (DefKind::Field, 70),
    (DefKind::Module, 142),
];

/// Definition nodes the store holds, by kind.
///
/// Lower than [`DEFS`] where one FQN is written more than once, which in
/// Elixir means one thing: a function with several clauses.
/// `Plug.Conn.Cookies#month_name/1` is written twelve times and is one
/// function. The pair of censuses is the point: the extractor's says nothing
/// was lost on the way in, the store's says nothing was lost or over-merged
/// on the way through.
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Function, 608),
    (DefKind::Field, 70),
    (DefKind::Module, 142),
];

/// Package nodes. **Zero, and asserted rather than observed.**
///
/// Elixir has no container above a module: `Plug.Conn` is one atom and is not
/// a member of anything called `Plug`. So every module is a *definition*
/// node, which is what makes two declarations of one module countable — the
/// finding Scala recorded for `object`, in Elixir's spelling.
const PACKAGES: u64 = 0;

/// Distinct FQNs a definition in more than one file claims. **Zero.**
///
/// And zero collisions with it: every identity this corpus declares twice is
/// a multi-clause function in one file, which `Resolver::mergeable` calls one
/// entity. `Plug.MixProject#plug_crypto_version/0` is the sharpest case —
/// `mix.exs` writes it once per branch of an `if`, both arms are read, and
/// both are the same function.
const COLLISIONS: u64 = 0;

/// Every module the corpus names that this repository does not declare.
///
/// **The single most important list in this file.** `External` sits outside
/// both terms of the resolution rate, so an in-repository module filed here
/// vanishes from the measurement rather than failing it — the
/// external-laundering finding, which the earlier tier-2 batches paid for
/// once already. Pinning the set by name means a composition bug shows up as
/// an addition here.
///
/// Three groups, and every one of them is genuinely somebody else's:
/// Elixir's own standard library and OTP (`Application`, `Config`, `EEx`,
/// `GenServer`, `Logger`, `Record`, `Supervisor`), the `ExUnit` and `Mix`
/// applications that ship with Elixir but are not this library, and the
/// `plug_crypto` hex dependency (`Plug.Crypto.*`). That last group is the
/// reason the external node is the *whole* module name: this repository
/// declares `Plug` and does not declare `Plug.Crypto`, and in Elixir those
/// two facts have nothing to do with each other.
const EXTERNALS: &[&str] = &[
    "Application",
    "Config",
    "EEx",
    "ExUnit.CaptureLog",
    "ExUnit.Case",
    "GenServer",
    "Logger",
    "Mix.Project",
    "Plug.Crypto.KeyGenerator",
    "Plug.Crypto.MessageEncryptor",
    "Plug.Crypto.MessageVerifier",
    "Record",
    "Supervisor",
];

/// Named nodes, spelled out: `(fqn, kind, declaring file, line, sites)`.
///
/// A census pins the scale; these pin the *shape*. Four of them are module
/// names the source never writes, and none of those can be right unless the
/// enclosing `defmodule`s were composed.
const PINNED: &[(&str, NodeKind, &str, u32, usize)] = &[
    // The corpus provenance's own example. `alias
    // Plug.CSRFProtection.InvalidCSRFTokenError` resolves here, and the
    // declaration site reads `defmodule InvalidCSRFTokenError do` — grep the
    // file for the composed name and you find nothing.
    (
        "Plug.CSRFProtection.InvalidCSRFTokenError",
        NodeKind::Definition(DefKind::Module),
        "lib/plug/csrf_protection.ex",
        119,
        1,
    ),
    // A protocol is a module, and so is each of its implementations.
    // `defimpl P, for: T` declares `P.T`, absolutely: the module it is
    // *written* inside contributes nothing.
    (
        "Plug.Exception",
        NodeKind::Definition(DefKind::Module),
        "lib/plug/exceptions.ex",
        4,
        1,
    ),
    (
        "Plug.Exception.Any",
        NodeKind::Definition(DefKind::Module),
        "lib/plug/exceptions.ex",
        50,
        1,
    ),
    (
        "Inspect.Plug.Conn",
        NodeKind::Definition(DefKind::Module),
        "lib/plug/conn.ex",
        2025,
        1,
    ),
    // Two compositions at once: the `for:` target is a nested module reached
    // through the alias `defmodule` created for it, and the impl name is the
    // protocol concatenated with the result. Neither string is in the file.
    (
        "Plug.Exception.Plug.DebuggerTest.ActionableError",
        NodeKind::Definition(DefKind::Module),
        "test/plug/debugger_test.exs",
        16,
        1,
    ),
    // A macro: exported, and not present at runtime.
    (
        "Plug.Builder#__using__/1",
        NodeKind::Definition(DefKind::Function),
        "lib/plug/builder.ex",
        148,
        1,
    ),
    // A two-clause function: one node, two declaration sites.
    (
        "Plug.Conn#put_resp_header/3",
        NodeKind::Definition(DefKind::Function),
        "lib/plug/conn.ex",
        913,
        2,
    ),
    // A `defp` written once per branch of an `if`, in the corpus's own
    // manifest. Both arms are read and both are the same function — the
    // Elixir spelling of the case C# recorded for `#if`.
    (
        "Plug.MixProject#plug_crypto_version/0",
        NodeKind::Definition(DefKind::Function),
        "mix.exs",
        58,
        2,
    ),
    // A struct key, which shares its module with the functions beside it and
    // cannot collide with a zero-arity one.
    (
        "Plug.Conn#%host",
        NodeKind::Definition(DefKind::Field),
        "lib/plug/conn.ex",
        225,
        1,
    ),
    (
        "Plug.Parsers.RequestTooLargeError#%plug_status",
        NodeKind::Definition(DefKind::Field),
        "lib/plug/parsers.ex",
        10,
        1,
    ),
];

/// Files whose directives are *all* inside a string literal, and the
/// line-oriented count that disagrees.
///
/// The corpus provenance counts directive lines; this track reads source.
/// Both are right about different questions, and the difference is named here
/// so that nobody has to rediscover it from a 27-reference gap.
const DOCUMENTED_ONLY: &[(&str, usize)] = &[
    ("lib/plug/error_handler.ex", 2),
    ("lib/plug/telemetry.ex", 1),
];

#[test]
fn the_elixir_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
        return;
    }
    let walked = source_files::<ElixirLang>(corpus).expect("walking the corpus");
    assert_eq!(walked.len(), FILES, "the walk found a different file set");

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_elixir(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Elixir.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "elixir       resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    for (code, count) in &tally.unresolved {
        println!("             {} {count}", reason_name(*code));
        reasons.insert(reason_name(*code).to_string(), *count);
    }

    // -- completeness -----------------------------------------------------

    // Independently re-extracted: the same files the scan owned, read again
    // from disk and put through the extractor with no resolver in sight. The
    // scan's buckets must account for every one of those references and for
    // nothing else.
    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    drop(store);
    assert_eq!(owned.len(), FILES, "the scan owned a different file set");

    let mut re_extracted = 0u64;
    let mut forms: BTreeMap<String, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    let mut composed = 0u64;
    let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        let facts = extract(rel, &source);
        re_extracted += facts.refs.len() as u64;
        per_file.insert(rel.as_str(), facts.refs.len());
        for r in &facts.refs {
            // The tier-2 contract, checked on real code and not only on a
            // fixture: a call or type reference here would put references
            // into a denominator this track cannot resolve.
            assert_eq!(r.kind, RefKind::Import, "{rel}: {}", r.raw_target);
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
            assert!(r.argc.is_none(), "{rel}: {}", r.raw_target);
        }
        // A directive and its reference are paired by span, so a clause with
        // no reference would be a silently dropped import.
        assert_eq!(
            facts.header.imports.len(),
            facts.refs.len(),
            "{rel}: directive clauses and import references disagree",
        );
        for spec in &facts.header.imports {
            *forms
                .entry(format!("{} {}", spec.directive.name(), spec.form.name()))
                .or_default() += 1;
        }
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
            // A module whose declared name is not the name it is filed
            // under: the composition a nested `defmodule` performs, and the
            // thing a reference in another file actually names.
            if d.kind == DefKind::Module && !d.owner.is_empty() {
                composed += 1;
            }
        }
    }
    println!("             forms {forms:?}");
    println!("             defs  {kinds:?} composed {composed}");

    let accounted =
        measured.resolved + measured.external + measured.local_binding + measured.unresolved;
    assert_eq!(
        accounted,
        re_extracted,
        "{re_extracted} references were extracted from {} files but {accounted} were accounted \
         for; a resolver that drops a reference reports a better rate for less work",
        owned.len(),
    );

    // -- the definitions, exactly ------------------------------------------

    let want: BTreeMap<u8, u64> = DEFS.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        kinds, want,
        "the definition census moved; tier 2's own deliverable is half \
         definitions and no rate can see them",
    );
    // 59 modules are declared under a name their own source never writes.
    // If this dropped to zero every one of them would become `External` and
    // the rate would go *up*, which is precisely why it is asserted.
    assert_eq!(composed, 59, "the nested-module composition census moved");

    // -- the tally, exactly -----------------------------------------------

    assert_eq!(re_extracted, REFERENCES);
    let want: BTreeMap<String, u64> = FORMS.iter().map(|(f, n)| ((*f).to_string(), *n)).collect();
    assert_eq!(forms, want, "the directive census moved");

    // A parser does not read a string as source. Both of these files write
    // directives inside a `@moduledoc` heredoc and no directive outside one.
    for (rel, documented) in DOCUMENTED_ONLY {
        let source = std::fs::read_to_string(corpus.join(rel)).expect("reading the file");
        let lines = source
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                ["alias ", "import ", "require ", "use "]
                    .iter()
                    .any(|d| t.starts_with(d))
            })
            .count();
        assert_eq!(lines, *documented, "{rel}: the file itself moved");
        assert_eq!(
            per_file.get(rel.to_owned()).copied(),
            Some(0),
            "{rel}: a directive inside a doc string became a reference",
        );
    }

    assert_eq!(measured.resolved, 116);
    // Every module this corpus names that this repository does not declare —
    // Elixir's standard library, OTP, ExUnit, Mix, and the `plug_crypto`
    // dependency. 55 occurrences over the 13 modules pinned in `EXTERNALS`.
    assert_eq!(measured.external, 55);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The other bucket outside both rate terms is empty.
    assert_eq!(measured.local_binding, 0);
    assert_eq!(measured.unresolved, 1);

    // The whole of the miss, named. `lib/plug/router.ex` writes `require
    // unquote(target)` inside a `quote` block: the module is chosen when the
    // macro expands, and a build that does not expand macros cannot say which
    // one it is. It counts *against* the rate rather than leaving it, which
    // is the honest direction — `External` would have been the cheap answer
    // and it would have been a guess.
    assert_eq!(reasons.get("DynamicModuleSpecifier").copied(), Some(1));
    assert_eq!(
        reasons.len(),
        1,
        "an unexpected reason appeared: {reasons:?}"
    );

    // The size of the `External` bucket, stated as the number it costs rather
    // than left to be inferred from three counts. 55 of the 172 references
    // name modules outside this repository, so the rate is computed over 117
    // and not over 172 — and if every one of them counted *against* the rate,
    // the way Ruby's `require 'time'` does, this track would report 67.4%
    // instead of 99.1%. Both numbers are printed here because the difference
    // between them is exactly what `External` buys, and a reader of the
    // baseline is owed it. `track_elixir::resolve` carries the argument for
    // why Elixir gets the C# answer and Ruby does not: Elixir names modules
    // absolutely and searches nothing, so a miss against a complete
    // in-repository set is definitive rather than a load root got wrong.
    let measured_rate = resolution_rate(measured.resolved, measured.unresolved);
    let if_external_counted =
        resolution_rate(measured.resolved, measured.unresolved + measured.external);
    println!(
        "             rate {:.1}%  (if every external counted against it: {:.1}%)",
        measured_rate.expect("a rate") * 100.0,
        if_external_counted.expect("a rate") * 100.0,
    );
    assert_eq!(measured_rate, resolution_rate(116, 1));
    assert_eq!(if_external_counted, resolution_rate(116, 56));

    // -- the definitions the store kept, by kind and by name ---------------

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let mut packages = 0u64;
    let mut externals: BTreeSet<String> = BTreeSet::new();
    let mut multi_file: BTreeSet<String> = BTreeSet::new();
    read.for_each_node(|_, record| {
        match record {
            NodeRecord::Definition {
                kind,
                fqn,
                declarations,
                ..
            } => {
                *stored.entry(kind).or_default() += 1;
                let files: BTreeSet<&str> = declarations.iter().map(|d| d.file.as_str()).collect();
                if files.len() > 1 {
                    multi_file.insert(fqn);
                }
            }
            NodeRecord::Package { .. } => packages += 1,
            NodeRecord::External { package, .. } => {
                externals.insert(package);
            }
        }
        Ok(())
    })
    .expect("walking the node table");
    println!("             nodes {stored:?} packages {packages}");
    println!("             externals {externals:?}");
    let want: BTreeMap<u8, u64> = STORED.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "the stored definition census moved");
    assert_eq!(
        packages, PACKAGES,
        "Elixir has no container above a module, so it mints no package node",
    );

    // -- the external set, by name -----------------------------------------

    let want: BTreeSet<String> = EXTERNALS.iter().map(|e| (*e).to_string()).collect();
    assert_eq!(
        externals, want,
        "the external module set moved; an in-repository module filed here \
         leaves the measurement instead of failing it",
    );
    // Said twice, because it is the failure this list exists to catch: not
    // one external name is a module this repository declares.
    for name in EXTERNALS {
        assert!(
            definition(&read, &node_id(Domain::Elixir, name))
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .is_none(),
            "{name} is declared in this repository and was filed as external",
        );
    }
    // The other half of the same argument: declaring a prefix claims nothing
    // under it. `Plug` is ours and `Plug.Crypto.KeyGenerator` is not, and
    // both are true at once because an Elixir module name is one atom.
    assert!(
        definition(&read, &node_id(Domain::Elixir, "Plug"))
            .expect("probing Plug")
            .is_some(),
        "this repository declares `Plug`",
    );

    // -- the union over files ----------------------------------------------

    assert_eq!(
        multi_file.len() as u64,
        COLLISIONS,
        "a definition is declared in two files: {multi_file:?}",
    );
    assert_eq!(
        report.fqn_collisions, COLLISIONS,
        "an identity was declared twice and the language does not call it one entity",
    );

    // -- the named nodes ---------------------------------------------------

    for (fqn, kind, file, line, sites) in PINNED {
        let id = node_id(Domain::Elixir, fqn);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("{fqn} is not in the store"));
        assert_eq!(def.node.name, *fqn);
        assert_eq!(def.node.kind, *kind, "{fqn}");
        assert_eq!(def.declarations.len(), *sites, "{fqn}");
        let here: Vec<u32> = def
            .declarations
            .iter()
            .filter(|d| d.file == *file)
            .map(|d| d.line)
            .collect();
        assert!(
            here.contains(line),
            "{fqn} is not declared at {file}:{line} — {} site(s) in that file, at {here:?}",
            here.len(),
        );
    }
    drop(read);

    // -- the ratchet ------------------------------------------------------

    let text = std::fs::read_to_string(BASELINE).unwrap_or_else(|e| {
        panic!(
            "reading {BASELINE}: {e}; record it with \
             `arthron gate {CORPUS} --language elixir --baseline {BASELINE} --rebase --commit <sha>`"
        )
    });
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Elixir.name(),
        "{BASELINE} measures another language; rates are per language and never aggregated",
    );
    assert_eq!(
        baseline.corpus, CORPUS,
        "{BASELINE} was recorded from another corpus",
    );
    match evaluate(&baseline, &measured) {
        GateVerdict::Pass { improved } => {
            if improved {
                println!("gate: pass — improved on the baseline; re-base to move the ratchet");
            }
        }
        GateVerdict::Fail(failures) => {
            let joined: Vec<String> = failures.iter().map(ToString::to_string).collect();
            panic!("gate: FAIL\n  {}", joined.join("\n  "));
        }
        GateVerdict::Error(e) => panic!("gate: error — {e}"),
    }
}
