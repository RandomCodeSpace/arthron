//! Milestone acceptance for the EcmaScript track, and the ratchet that keeps
//! it from sliding.
//!
//! Two corpora, **two baselines, never one**: `arthron gate` compares per
//! language, and one combined EcmaScript number would let a collapse in
//! JavaScript be masked by TypeScript. Each baseline names the language it
//! measures and is compared only against that language's tally.
//!
//! The two baselines here are written by the command, never by hand:
//!
//! ```text
//! arthron gate corpus/javascript/fastify  --language javascript \
//!     --baseline baselines/javascript-fastify.toml  --rebase --commit <pin>
//! arthron gate corpus/typescript/vue-core --language typescript \
//!     --baseline baselines/typescript-vue-core.toml --rebase --commit <pin>
//! arthron gate corpus/javascript/express  --language javascript \
//!     --baseline baselines/javascript-express.toml --rebase --commit dbac741
//! arthron gate corpus/typescript/zod      --language typescript \
//!     --baseline baselines/typescript-zod.toml     --rebase --commit 1fb56a5
//! ```
//!
//! `--language` is load-bearing and the rendered header comment omits it: the
//! flag defaults to `go`, so re-running the printed command against one of
//! these files would overwrite a JavaScript or TypeScript baseline with the
//! Go tally. The `language = "…"` field records which one is meant, and
//! `a_baseline_is_refused_against_another_languages_scan` below is what makes
//! the mistake fail rather than pass quietly.
//!
//! The comparison also runs here, through the same `gate::evaluate` the
//! command uses, so CI gates this track without building the binary — the
//! arithmetic is identical and only the entry point differs.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Baseline, GateVerdict, Measured, evaluate, parse_baseline};
use arthron::lang::Language;
use arthron::model::{DefKind, Lang, node_id, reason_name};
use arthron::pipeline::source_files;
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Report, Store};
use arthron::track_ecma::extract::extract;
use arthron::track_ecma::lang::{Dialect, JsLang, TsLang};
use arthron::track_ecma::scan_ecma;

mod support;

/// Every EcmaScript corpus: `(root, language, baseline, the manifest whose
/// presence proves the corpus was cloned in, the exact unresolved reasons)`.
///
/// The reasons are a column here and not a floor anywhere, for the reason the
/// tier-2 tracks have always pinned theirs: the four numbers a baseline holds
/// are identical whichever reason each unresolved reference carries, so a
/// resolver may relabel every one of them and pass every gate in this file. A
/// floor — "`NeedsTypeInference` is above zero" — survives any relabelling
/// that leaves one reference behind.
///
/// The marker is per corpus and not a constant, because a package manifest is
/// not always at the root: zod is vendored as the `packages/zod/` member of a
/// monorepo, kept at its real repo-relative path so its `extends` chain still
/// reaches `.configs/tsconfig.base.json`. Looking for `package.json` at the
/// root there finds nothing and would skip the whole corpus silently, which is
/// a vacuous pass wearing a gate's clothes.
#[allow(clippy::type_complexity)]
const CORPORA: &[(&str, Lang, &str, &str, &[(&str, u64)])] = &[
    (
        "corpus/javascript/fastify",
        Lang::JavaScript,
        "baselines/javascript-fastify.toml",
        "package.json",
        &[
            ("DynamicModuleSpecifier", 1),
            ("ModuleNotFound", 10),
            ("NeedsExpressionType", 993),
            ("NeedsReceiverType", 102),
            ("NeedsTypeInference", 296),
            ("NoMatchingDefinition", 238),
        ],
    ),
    (
        "corpus/javascript/express",
        Lang::JavaScript,
        "baselines/javascript-express.toml",
        "package.json",
        // No `ModuleNotFound`: express vendors what it imports, and its
        // misses are expression-level instead.
        //
        // And **no `NoMatchingDefinition` at all**, which is the whole point
        // of the entry above it. Every one of the 1,728 this corpus used to
        // report was mocha putting a name in the universe scope — `it` 1,111,
        // `describe` 554, `before` 59, `after` 3 — plus one `XMLHttpRequest`
        // the host global list had left out. A reason whose contract says
        // "our bug, and near zero" now reads zero here, so the next
        // occurrence of it is a real one.
        &[
            ("DynamicModuleSpecifier", 3),
            ("NeedsExpressionType", 2906),
            ("NeedsReceiverType", 132),
            ("NeedsTypeInference", 781),
            ("UnknownPackage", 1730),
        ],
    ),
    (
        "corpus/typescript/vue-core",
        Lang::TypeScript,
        "baselines/typescript-vue-core.toml",
        "package.json",
        // 14,618 `UnknownPackage` where 15,276 `NoMatchingDefinition` used to
        // be: vitest's injected globals, 13,833 of them under the reason that
        // claimed the lookup table was complete, and 785 more — `vi.fn`,
        // `expect.any` — under the one that claimed a type was missing. What
        // is left in `NoMatchingDefinition` is 1,443, and it is real: DOM
        // interface names this build deliberately does not claim as host
        // globals, and members the resolver genuinely did not reach.
        &[
            ("ModuleNotFound", 108),
            ("NeedsExpressionType", 11151),
            ("NeedsReceiverType", 85),
            ("NeedsTypeInference", 535),
            ("NoMatchingDefinition", 1443),
            ("UnindexedSupertype", 5),
            ("UnknownPackage", 14618),
        ],
    ),
    (
        "corpus/typescript/zod",
        Lang::TypeScript,
        "baselines/typescript-zod.toml",
        "packages/zod/package.json",
        // The opposite shape to vue-core, and the reason both are gated:
        // resolution through `exports` conditions rather than a `paths`
        // mapping.
        //
        // `ModuleNotFound` was 7,822 and is 1. Every one of them was this
        // repository importing itself — `import * as z from "zod/v4"` — down
        // the `"types"` branch of its own `exports` map, which names a built
        // `.d.cts` beside the manifest that no scan of the sources can see.
        // The map's *first* branch is `"@zod/source"`, and
        // `packages/zod/tsconfig.json` states exactly that in
        // `compilerOptions.customConditions`. Reading it links 7,037 more
        // references into `packages/zod/src/`.
        //
        // `NoMatchingDefinition` rose 524 → 1,123 and `NeedsTypeInference`
        // 1,576 → 1,761 in the same movement, and neither is a new miss: a
        // module that could not be found gave every name reached through it
        // one answer, and a module that *is* found gives each of them its own
        // — including, for a namespace re-exported by name, an honest one
        // this resolver does not yet follow.
        &[
            ("ModuleNotFound", 1),
            ("NeedsExpressionType", 8811),
            ("NeedsReceiverType", 50),
            ("NeedsTypeInference", 1761),
            ("NoMatchingDefinition", 1123),
            ("UnindexedSupertype", 2),
            ("UnknownPackage", 8036),
        ],
    ),
];

/// The marker a corpus is recognised by, or a failure naming the corpus: a
/// root this file does not describe cannot be checked for presence at all.
fn marker_for(corpus: &Path) -> &'static str {
    let name = corpus.to_string_lossy().replace('\\', "/");
    CORPORA
        .iter()
        .find(|(root, ..)| *root == name)
        .map(|(_, _, _, marker, _)| *marker)
        .unwrap_or_else(|| panic!("{name} is not one of the corpora CORPORA describes"))
}

/// The exact unresolved reasons a corpus produces, or a failure naming it: a
/// root this file does not describe has no tally to compare against, which is
/// the same absence of a gate as a corpus with no baseline.
fn reasons_for(corpus: &Path) -> &'static [(&'static str, u64)] {
    let name = corpus.to_string_lossy().replace('\\', "/");
    CORPORA
        .iter()
        .find(|(root, ..)| *root == name)
        .map(|(_, _, _, _, reasons)| *reasons)
        .unwrap_or_else(|| panic!("{name} is not one of the corpora CORPORA describes"))
}

/// Whether a corpus has been cloned in.
///
/// It lives in RandomCodeSpace/arthron-corpus, cloned into ./corpus
/// (gitignored). Skipping is correct when it is absent — failing would make an
/// unfetched corpus look like a broken engine.
fn corpus_present(corpus: &Path) -> bool {
    if corpus.join(marker_for(corpus)).is_file() {
        return true;
    }
    support::missing(corpus);
    false
}

fn measure(corpus: &Path, lang: Lang) -> (Report, Measured) {
    let dir = tempfile::tempdir().unwrap();
    let report = scan_ecma(corpus, &dir.path().join("graph.redb")).expect("scan");
    let tally = report
        .per_lang
        .get(&lang.code())
        .unwrap_or_else(|| panic!("{} has no line in the report", lang.name()))
        .clone();
    let measured = Measured {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "{}: resolved {} external {} local-binding {} unresolved {}",
        lang.name(),
        measured.resolved,
        measured.external,
        measured.local_binding,
        measured.unresolved,
    );
    for (code, count) in &tally.unresolved {
        println!("  {}: {count}", reason_name(*code));
    }
    if let Some(rate) = arthron::resolution_rate(measured.resolved, measured.unresolved) {
        println!("  rate {:.1}%", rate * 100.0);
    }
    (report, measured)
}

fn baseline(path: &str) -> Baseline {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    parse_baseline(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Compare against the committed baseline and fail on any regression — or on
/// any drift in `external` or `local_binding`, which sit outside *both* terms
/// of the rate and are therefore the one way this gate could be raised without
/// anything being linked.
fn gate(corpus: &Path, lang: Lang, baseline_path: &str) {
    if !corpus_present(corpus) {
        return;
    }
    let (report, measured) = measure(corpus, lang);
    // Exactly, before the baseline is even read: `evaluate` below compares
    // four integers and every one of them survives a relabelled reason.
    support::assert_reasons(
        &corpus.to_string_lossy(),
        &report.per_lang[&lang.code()].unresolved,
        reasons_for(corpus),
    );
    let b = baseline(baseline_path);
    assert_eq!(
        b.language,
        lang.name(),
        "{baseline_path} measures another language"
    );
    assert_eq!(
        b.corpus,
        corpus.to_string_lossy().replace('\\', "/"),
        "{baseline_path} was recorded from another corpus",
    );
    match evaluate(&b, &measured) {
        GateVerdict::Pass { improved } => {
            if improved {
                println!("improved over {baseline_path} — re-base deliberately");
            }
        }
        GateVerdict::Fail(failures) => {
            panic!("{baseline_path}: {failures:?}\nmeasured {measured:?}")
        }
        GateVerdict::Error(e) => panic!("{baseline_path}: {e}"),
    }
}

#[test]
fn javascript_holds_its_baseline_on_fastify() {
    gate(
        Path::new("corpus/javascript/fastify"),
        Lang::JavaScript,
        "baselines/javascript-fastify.toml",
    );
}

#[test]
fn typescript_holds_its_baseline_on_vue_core() {
    gate(
        Path::new("corpus/typescript/vue-core"),
        Lang::TypeScript,
        "baselines/typescript-vue-core.toml",
    );
}

#[test]
fn javascript_holds_its_baseline_on_express() {
    // The second JavaScript corpus. express 5 declares no `main`, no `type`,
    // no `exports` and no `module` at all, so its 25 directory specifiers
    // reach the package root only through Node's implicit `index.js` — a rule
    // fastify's tree never asks the resolver to apply.
    gate(
        Path::new("corpus/javascript/express"),
        Lang::JavaScript,
        "baselines/javascript-express.toml",
    );
}

#[test]
fn typescript_holds_its_baseline_on_zod() {
    // The second TypeScript corpus, and the opposite idiom to vue-core on
    // every axis that decides module resolution: no `paths` mapping at all,
    // `"module": "nodenext"` with extension-ful relative specifiers, and
    // resolution through `exports` conditions.
    gate(
        Path::new("corpus/typescript/zod"),
        Lang::TypeScript,
        "baselines/typescript-zod.toml",
    );
}

#[test]
fn every_ecmascript_corpus_has_its_own_baseline() {
    // Four corpora, four baselines, and no two of them the same file: one
    // number covering two repositories would let a collapse in either hide
    // behind the other, exactly as one number covering two languages would.
    let mut seen: Vec<&str> = CORPORA.iter().map(|(_, _, path, ..)| *path).collect();
    seen.sort_unstable();
    let count = seen.len();
    seen.dedup();
    assert_eq!(seen.len(), count, "two corpora share a baseline file");
    for (root, lang, path, ..) in CORPORA {
        let b = baseline(path);
        assert_eq!(b.language, lang.name(), "{path} measures another language");
        assert_eq!(b.corpus, *root, "{path} was recorded from another corpus");
    }
}

#[test]
fn a_baseline_is_refused_against_another_languages_scan() {
    // The rule that keeps two rates from becoming one: a baseline names the
    // language it measures, and nothing may compare it against another's.
    let js = baseline("baselines/javascript-fastify.toml");
    let ts = baseline("baselines/typescript-vue-core.toml");
    assert_eq!(js.language, "javascript");
    assert_eq!(ts.language, "typescript");
    assert_ne!(js.language, ts.language);
    // And no third file aggregates them.
    assert!(
        !Path::new("baselines/ecmascript.toml").exists(),
        "a combined EcmaScript baseline would let one language mask the other",
    );
}

#[test]
fn the_unresolved_floor_is_real_on_both_corpora() {
    // The reasons that must stay large. `NeedsTypeInference` and its two
    // siblings are the honest cost of not running a type checker; a scan that
    // reported them near zero would have moved them somewhere they do not
    // belong — `LocalBinding` and `External` are outside *both* rate terms, so
    // routing anything into them raises the rate without linking a thing.
    for (corpus, lang) in [
        (Path::new("corpus/javascript/fastify"), Lang::JavaScript),
        (Path::new("corpus/typescript/vue-core"), Lang::TypeScript),
    ] {
        if !corpus_present(corpus) {
            continue;
        }
        let (report, measured) = measure(corpus, lang);
        let tally = &report.per_lang[&lang.code()];
        let inference: u64 = tally
            .unresolved
            .iter()
            .filter(|(code, _)| {
                matches!(
                    reason_name(**code),
                    "NeedsTypeInference" | "NeedsReceiverType" | "NeedsExpressionType"
                )
            })
            .map(|(_, count)| *count)
            .sum();
        assert!(
            inference > 0,
            "{}: a receiver-type floor of zero would mean it was reclassified",
            lang.name(),
        );
        assert!(measured.resolved > 0, "{}: nothing linked", lang.name());
        assert!(
            measured.unresolved > 0,
            "{}: a scan claiming everything resolved is lying somewhere",
            lang.name(),
        );
    }
}

// -- the definition census -------------------------------------------------
//
// Every assertion above this line is about references, and none of them can
// see a definition go missing. Deleting the rule that emits
// `DefKind::Method` takes 307 nodes out of vue-core and moves no rate, no
// bucket and neither baseline — the references that named them change
// *reason*, and the reasons here are floors rather than counts. So the
// definitions are counted exactly, on both sides of the store, per corpus.

/// The measurement one EcmaScript corpus's census is.
struct Census {
    /// The corpus root, which is also its key in [`CORPORA`].
    corpus: &'static str,
    /// Files the scan owned. A JavaScript corpus has no `.ts` and a
    /// TypeScript one has no `.js`, which is why one dialect's walk is the
    /// whole file set — asserted below rather than assumed.
    files: usize,
    defs: &'static [(DefKind, u64)],
    stored: &'static [(DefKind, u64)],
    packages: u64,
    externals: u64,
    pinned: &'static [(&'static str, NodeKind, &'static str, u32)],
}

/// fastify: 260 JavaScript files, CommonJS throughout.
const FASTIFY: Census = Census {
    corpus: "corpus/javascript/fastify",
    files: 260,
    // `Const` dominates because that is what CommonJS module scope is: 1361
    // `const x = require(…)` and `const y = …` bindings. `Module` is one
    // per file.
    defs: &[
        (DefKind::Function, 218),
        (DefKind::Method, 61),
        (DefKind::Type, 3),
        (DefKind::Const, 1361),
        (DefKind::Var, 12),
        (DefKind::Constructor, 3),
        (DefKind::Field, 253),
        (DefKind::Property, 7),
        (DefKind::Module, 260),
        (DefKind::Alias, 71),
    ],
    stored: &[
        (DefKind::Function, 218),
        (DefKind::Method, 61),
        (DefKind::Type, 3),
        (DefKind::Const, 1361),
        (DefKind::Var, 12),
        (DefKind::Constructor, 3),
        (DefKind::Field, 253),
        (DefKind::Property, 7),
        (DefKind::Alias, 70),
    ],
    // One per file: an EcmaScript module *is* a file.
    packages: 260,
    externals: 60,
    pinned: &[
        (
            "lib/reply.js#value:Reply",
            NodeKind::Definition(DefKind::Function),
            "lib/reply.js",
            66,
        ),
        // A prototype assignment, filed under the constructor function it
        // extends rather than beside it.
        (
            "lib/reply.js#value:Reply.prototype.code",
            NodeKind::Definition(DefKind::Method),
            "lib/reply.js",
            334,
        ),
        (
            "lib/reply.js#value:CONTENT_TYPE",
            NodeKind::Definition(DefKind::Const),
            "lib/reply.js",
            42,
        ),
        // A member of the object literal above: a field of a const, which is
        // the shape a `module.exports = { … }` surface is made of.
        (
            "lib/reply.js#value:CONTENT_TYPE.JSON",
            NodeKind::Definition(DefKind::Field),
            "lib/reply.js",
            43,
        ),
        (
            "lib/reply.js#value:buildReply",
            NodeKind::Definition(DefKind::Function),
            "lib/reply.js",
            1001,
        ),
    ],
};

/// express: 142 JavaScript files, and the tree with no `main`, no `type`,
/// no `exports` and no `module` in its manifest.
const EXPRESS: Census = Census {
    corpus: "corpus/javascript/express",
    files: 142,
    // `var` where fastify has `const`: express 5 is still written in the
    // older idiom, which is the reason the two JavaScript corpora are not
    // one.
    defs: &[
        (DefKind::Function, 92),
        (DefKind::Method, 4),
        (DefKind::Const, 16),
        (DefKind::Var, 463),
        (DefKind::Field, 19),
        (DefKind::Module, 142),
        (DefKind::Alias, 77),
    ],
    stored: &[
        (DefKind::Function, 92),
        (DefKind::Method, 4),
        (DefKind::Const, 16),
        (DefKind::Var, 463),
        (DefKind::Field, 19),
        (DefKind::Alias, 74),
    ],
    packages: 142,
    externals: 54,
    pinned: &[
        (
            "lib/application.js#value:tryRender",
            NodeKind::Definition(DefKind::Function),
            "lib/application.js",
            625,
        ),
        (
            "lib/application.js#value:Router",
            NodeKind::Definition(DefKind::Var),
            "lib/application.js",
            26,
        ),
        // The module's default export, which is what a directory specifier
        // reaching this file through Node's implicit `index.js` binds.
        (
            "lib/application.js#value:*default*",
            NodeKind::Definition(DefKind::Const),
            "lib/application.js",
            40,
        ),
        (
            "lib/express.js#value:*default*.json",
            NodeKind::Definition(DefKind::Field),
            "lib/express.js",
            77,
        ),
    ],
};

/// vue-core: 483 TypeScript files, a `paths`-mapped monorepo.
const VUE_CORE: Census = Census {
    corpus: "corpus/typescript/vue-core",
    files: 483,
    // 496 modules over 483 files: TypeScript's `declare module` and
    // `namespace` blocks declare containers a file does not.
    defs: &[
        (DefKind::Function, 1251),
        (DefKind::Method, 307),
        (DefKind::Type, 738),
        (DefKind::Const, 1319),
        (DefKind::Var, 90),
        (DefKind::Constructor, 16),
        (DefKind::Field, 2035),
        (DefKind::Property, 13),
        (DefKind::Module, 496),
        (DefKind::Alias, 567),
    ],
    stored: &[
        (DefKind::Function, 1172),
        (DefKind::Method, 301),
        (DefKind::Type, 738),
        (DefKind::Const, 1319),
        (DefKind::Var, 90),
        (DefKind::Constructor, 16),
        (DefKind::Field, 2035),
        (DefKind::Property, 9),
        (DefKind::Alias, 567),
    ],
    packages: 495,
    externals: 44,
    pinned: &[
        (
            "packages/reactivity/src/effect.ts#value:ReactiveEffect.prototype.notify",
            NodeKind::Definition(DefKind::Method),
            "packages/reactivity/src/effect.ts",
            150,
        ),
        // A `get`/`set` pair on the same class: an accessor, and neither a
        // method nor a field.
        (
            "packages/reactivity/src/effect.ts#value:ReactiveEffect.prototype.dirty",
            NodeKind::Definition(DefKind::Property),
            "packages/reactivity/src/effect.ts",
            225,
        ),
        (
            "packages/reactivity/src/effect.ts#value:batch",
            NodeKind::Definition(DefKind::Function),
            "packages/reactivity/src/effect.ts",
            251,
        ),
        // A member of an `enum`, in the type space rather than the value
        // space — the `type:` tag on the container says which.
        (
            "packages/reactivity/src/effect.ts#type:EffectFlags.ACTIVE",
            NodeKind::Definition(DefKind::Const),
            "packages/reactivity/src/effect.ts",
            45,
        ),
        (
            "packages/reactivity/src/effect.ts#type:DebuggerOptions.onTrack",
            NodeKind::Definition(DefKind::Field),
            "packages/reactivity/src/effect.ts",
            24,
        ),
    ],
};

/// zod: 287 TypeScript files, `"module": "nodenext"`, no `paths` mapping.
const ZOD: Census = Census {
    corpus: "corpus/typescript/zod",
    files: 287,
    defs: &[
        (DefKind::Function, 730),
        (DefKind::Method, 426),
        (DefKind::Type, 1199),
        (DefKind::Const, 910),
        (DefKind::Var, 4),
        (DefKind::Constructor, 10),
        (DefKind::Field, 1129),
        (DefKind::Property, 73),
        (DefKind::Module, 302),
        (DefKind::Alias, 371),
    ],
    stored: &[
        (DefKind::Function, 677),
        (DefKind::Method, 411),
        (DefKind::Type, 1199),
        (DefKind::Const, 910),
        (DefKind::Var, 4),
        (DefKind::Constructor, 10),
        (DefKind::Field, 1129),
        (DefKind::Property, 72),
        (DefKind::Alias, 371),
    ],
    packages: 293,
    externals: 7,
    pinned: &[
        (
            "packages/zod/src/v4/core/schemas.ts#value:$ZodAny",
            NodeKind::Definition(DefKind::Const),
            "packages/zod/src/v4/core/schemas.ts",
            1445,
        ),
        // One name, two spaces: `$ZodFunction` is an interface in the type
        // space and a const in the value space, and a member of the first is
        // not a member of the second.
        (
            "packages/zod/src/v4/core/schemas.ts#type:$ZodFunction.implement",
            NodeKind::Definition(DefKind::Method),
            "packages/zod/src/v4/core/schemas.ts",
            4397,
        ),
        (
            "packages/zod/src/v4/core/schemas.ts#value:getTupleOptStart",
            NodeKind::Definition(DefKind::Function),
            "packages/zod/src/v4/core/schemas.ts",
            2751,
        ),
        (
            "packages/zod/src/v4/core/schemas.ts#type:$InferEnumInput",
            NodeKind::Definition(DefKind::Type),
            "packages/zod/src/v4/core/schemas.ts",
            3195,
        ),
    ],
};

/// Count the definitions on both sides of the store and compare them with
/// what this corpus's [`Census`] records.
fn assert_census(census: &Census) {
    let root = Path::new(census.corpus);
    if !corpus_present(root) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.redb");
    scan_ecma(root, &db).expect("scan");

    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    drop(store);
    assert_eq!(
        owned.len(),
        census.files,
        "{}: the scan owned a different file set",
        census.corpus,
    );

    // Re-extracted from disk, each dialect over its own walk. Both are run
    // on every corpus: a `.ts` file appearing in a JavaScript tree, or the
    // reverse, would change which rules read it, and summing two walks that
    // must not overlap is how that shows up as a moved census rather than
    // silently.
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    let mut walked = 0usize;
    for (dialect, paths) in [
        (
            Dialect::JavaScript,
            source_files::<JsLang>(root).expect("walking the corpus"),
        ),
        (
            Dialect::TypeScript,
            source_files::<TsLang>(root).expect("walking the corpus"),
        ),
    ] {
        for path in &paths {
            let rel = path
                .strip_prefix(root)
                .expect("a walked path is under the corpus")
                .to_string_lossy()
                .replace('\\', "/");
            let source =
                std::fs::read_to_string(path).unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
            walked += 1;
            for def in &extract(dialect, &rel, &source).defs {
                *kinds.entry(def.kind.code()).or_default() += 1;
            }
        }
    }
    assert_eq!(
        walked, census.files,
        "{}: the two dialect walks do not partition the scan's file set",
        census.corpus,
    );
    println!("{}: extracted defs {kinds:?}", census.corpus);
    let want: BTreeMap<u8, u64> = census.defs.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        kinds, want,
        "{}: the definition census moved, and no rate can see it",
        census.corpus,
    );

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let (mut packages, mut externals) = (0u64, 0u64);
    read.for_each_node(|_, record| {
        match record {
            NodeRecord::Definition { kind, .. } => *stored.entry(kind).or_default() += 1,
            NodeRecord::Package { .. } => packages += 1,
            NodeRecord::External { .. } => externals += 1,
        }
        Ok(())
    })
    .expect("walking the node table");
    println!(
        "{}: stored defs {stored:?} packages {packages} externals {externals}",
        census.corpus,
    );
    let want: BTreeMap<u8, u64> = census.stored.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        stored, want,
        "{}: the stored definition census moved",
        census.corpus,
    );
    assert_eq!(
        packages, census.packages,
        "{}: the stored package census moved",
        census.corpus,
    );
    assert_eq!(
        externals, census.externals,
        "{}: the stored external census moved",
        census.corpus,
    );

    for (fqn, kind, file, line) in census.pinned {
        // JavaScript and TypeScript share one identity space, so the domain
        // is the family's and not the dialect's.
        let id = node_id(Lang::JavaScript.domain(), fqn);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("{fqn} is not in the store"));
        assert_eq!(def.node.name, *fqn);
        assert_eq!(def.node.kind, *kind, "{fqn}");
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
}

/// One reference row, named exactly: `(file, raw_target, enclosing)` → what
/// the resolver concluded, rendered the way this file's `outcome` renders it.
///
/// A tally cannot tell a corpus which rows moved, and the three changes this
/// commit makes are each visible as a *reason on a named site*. Pinning the
/// sites is what makes a regression fail by name rather than by an integer
/// that has to be re-derived.
const PINNED_ROWS: &[(&str, &str, &str, &str)] = &[
    // mocha puts `it` in express's universe scope. Not this repository's
    // missing definition, and not `External` either — still `Unresolved`, so
    // the reference stays in both terms of the rate.
    (
        "corpus/javascript/express",
        "test/Route.js",
        "it",
        "unresolved UnknownPackage",
    ),
    // The host global list had left this one out, so a name no repository
    // ever declares reported `NoMatchingDefinition` — the reason that says
    // the table was complete.
    (
        "corpus/javascript/express",
        "examples/search/public/client.js",
        "XMLHttpRequest",
        "external web:global",
    ),
    // vitest's, through `globals: true`, and the same answer this resolver
    // already gives zod for `import { expect } from "vitest"`.
    (
        "corpus/typescript/vue-core",
        "packages/compiler-core/__tests__/codegen.spec.ts",
        "expect",
        "unresolved UnknownPackage",
    ),
    // `customConditions`. Without `"@zod/source"` in the set this took the
    // `"types"` branch of zod's own `exports` map and missed on a built
    // artefact; with it, the import lands on the source file in this tree.
    (
        "corpus/typescript/zod",
        "packages/zod/src/v4/classic/tests/anyunknown.test.ts",
        "zod/v4",
        "resolved packages/zod/src/v4/index.ts",
    ),
];

/// Every row in [`PINNED_ROWS`] says what it is supposed to say.
#[test]
fn the_three_reclassifications_are_pinned_to_named_sites() {
    let mut by_corpus: BTreeMap<&str, Vec<&(&str, &str, &str, &str)>> = BTreeMap::new();
    for pin in PINNED_ROWS {
        by_corpus.entry(pin.0).or_default().push(pin);
    }
    for (root, pins) in by_corpus {
        let corpus = Path::new(root);
        if !corpus_present(corpus) {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.redb");
        scan_ecma(corpus, &db).expect("scan");
        let store = Store::open(&db).expect("store opens");
        let snapshot = store.snapshot().expect("snapshot");
        for (_, file, target, want) in pins {
            let found: Vec<String> = snapshot
                .rows
                .iter()
                .filter(|(key, _)| key.file == *file && key.raw_target == *target)
                .map(|(_, record)| match &record.outcome {
                    arthron::store::StoredOutcome::Resolved(id) => {
                        match store.node(id).expect("node read") {
                            Some(NodeRecord::Definition { fqn, .. }) => format!("resolved {fqn}"),
                            Some(NodeRecord::Package { import_path, .. }) => {
                                format!("resolved {import_path}")
                            }
                            _ => "resolved <dangling>".to_string(),
                        }
                    }
                    arthron::store::StoredOutcome::External(p) => format!("external {p}"),
                    arthron::store::StoredOutcome::Unresolved(c) => {
                        format!("unresolved {}", reason_name(*c))
                    }
                })
                .collect();
            assert!(
                !found.is_empty(),
                "{root}: no row for ({file}, {target}) — a pinned site vanished",
            );
            for outcome in &found {
                assert_eq!(outcome, want, "{root}: ({file}, {target})");
            }
        }
    }
}

#[test]
fn the_fastify_definition_census_is_exact() {
    assert_census(&FASTIFY);
}

#[test]
fn the_express_definition_census_is_exact() {
    assert_census(&EXPRESS);
}

#[test]
fn the_vue_core_definition_census_is_exact() {
    assert_census(&VUE_CORE);
}

#[test]
fn the_zod_definition_census_is_exact() {
    assert_census(&ZOD);
}

/// Every corpus in [`CORPORA`] has a census, and every census names a corpus
/// that is in it.
///
/// The gap this closes is the one `every_committed_baseline_is_gated_by_a_test`
/// closes one level up: a fifth corpus could be added to `CORPORA`, gated by
/// a rate, and have no definition census at all — which is exactly the state
/// this file was in.
#[test]
fn every_ecmascript_corpus_has_a_definition_census() {
    let censuses = [&FASTIFY, &EXPRESS, &VUE_CORE, &ZOD];
    let mut listed: Vec<&str> = censuses.iter().map(|c| c.corpus).collect();
    listed.sort_unstable();
    let mut described: Vec<&str> = CORPORA.iter().map(|(root, ..)| *root).collect();
    described.sort_unstable();
    assert_eq!(
        listed, described,
        "a corpus with a rate and no definition census is gated on half of what it delivers",
    );
}

/// Count one language's references in the corpus by extracting it again,
/// independently of the pipeline.
///
/// This deliberately does not ask the pipeline how many references it found:
/// a bug that loses one between the extractor and the store would lose it
/// from both sides of the comparison and the assertion would pass. It shares
/// only the two things it must in order to be counting the same files at all
/// — `extract`, and `source_files` for the file set.
fn extracted_reference_count<L: Language>(corpus: &Path, dialect: Dialect) -> u64 {
    let mut total = 0u64;
    for path in source_files::<L>(corpus).expect("walking the corpus") {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        total += extract(dialect, &rel, &source).refs.len() as u64;
    }
    total
}

/// Both languages' columns must partition both languages' references, on one
/// scan of one corpus.
fn assert_every_reference_is_accounted_for(corpus: &Path) {
    if !corpus_present(corpus) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let report = scan_ecma(corpus, &dir.path().join("graph.redb")).expect("scan");
    // Both halves, on every corpus. A corpus with no file of one language
    // still asserts something: `0 == 0` fails the moment a row is tagged with
    // a language whose files are not there, which is exactly how one rate
    // could start borrowing from the other's.
    for (lang, extracted) in [
        (
            Lang::JavaScript,
            extracted_reference_count::<JsLang>(corpus, Dialect::JavaScript),
        ),
        (
            Lang::TypeScript,
            extracted_reference_count::<TsLang>(corpus, Dialect::TypeScript),
        ),
    ] {
        let tally = report
            .per_lang
            .get(&lang.code())
            .cloned()
            .unwrap_or_default();
        let stored =
            tally.resolved + tally.external + tally.local_binding + tally.unresolved_total();
        println!(
            "{} on {}: stored outcomes {stored}, extracted references {extracted}",
            lang.name(),
            corpus.display(),
        );
        assert_eq!(
            stored,
            extracted,
            "{} on {}: resolved {} + external {} + local-binding {} + unresolved {} \
             must equal the {extracted} references the extractor found — every \
             reference gets exactly one stored outcome",
            lang.name(),
            corpus.display(),
            tally.resolved,
            tally.external,
            tally.local_binding,
            tally.unresolved_total(),
        );
    }
}

#[test]
fn every_reference_on_fastify_has_exactly_one_stored_outcome() {
    // "The resolver never drops" is the project's central claim, and a rate is
    // no evidence for it: silently discarding the references it cannot link
    // would *raise* the rate. The four reported columns partition the
    // extracted references, so their sum is the reference count — exactly.
    // Under-counting is a dropped reference; over-counting is one reference
    // reported as two outcomes. Both break the contract.
    //
    // `local_binding` is one of the columns even though it is outside both
    // terms of the rate: it is excluded from the *measurement*, never from the
    // *accounting*. Leaving it out here is precisely how moving references
    // into it could look like an improvement.
    assert_every_reference_is_accounted_for(Path::new("corpus/javascript/fastify"));
}

#[test]
fn every_reference_on_vue_core_has_exactly_one_stored_outcome() {
    assert_every_reference_is_accounted_for(Path::new("corpus/typescript/vue-core"));
}

/// Every triple-slash reference directive in a file, found by reading the
/// source text rather than by asking the extractor.
///
/// An **independent oracle**, and the point of it is what it does not share:
/// `extracted_reference_count` above re-runs the production extractor, so a
/// reference the front end never emits is missing from both sides of that
/// comparison and the assertion passes anyway. That is exactly how
/// `/// <reference … />` went unnoticed — a directive is a comment, no rule
/// selected it, and no bucket ever received it. This function knows only that
/// a directive is a line beginning `///` that names `path=` or `types=`.
fn directives_in(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in source.lines() {
        let line = line.trim_start();
        if !line.starts_with("///") || !line.contains("<reference") {
            continue;
        }
        for attribute in ["path=", "types="] {
            let Some(rest) = line.split_once(attribute).map(|(_, r)| r) else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                continue;
            };
            if let Some(value) = rest[1..].split(quote).next()
                && !value.is_empty()
            {
                found.push(value.to_string());
            }
        }
    }
    found
}

#[test]
fn every_reference_directive_in_the_corpus_is_extracted() {
    // A18. The never-drop guarantee is a claim about phase two *and* about
    // the front end: a reference nothing emits reaches no bucket at all, and
    // no per-reason tally can show its absence.
    let corpus = Path::new("corpus/typescript/vue-core");
    if !corpus_present(corpus) {
        return;
    }
    let mut checked = 0usize;
    for path in source_files::<TsLang>(corpus).expect("walking the corpus") {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let expected = directives_in(&source);
        if expected.is_empty() {
            continue;
        }
        let facts = extract(Dialect::TypeScript, &rel, &source);
        for specifier in expected {
            assert!(
                facts
                    .header
                    .imports
                    .iter()
                    .any(|i| i.specifier.as_deref() == Some(specifier.as_str())),
                "{rel}: `{specifier}` is written in the source and named by no reference",
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "the corpus carries no directive, so this proves nothing — say so \
         rather than letting a vacuous pass stand in for a measurement",
    );
}
