//! Acceptance for the Lua track against the busted corpus: nothing is
//! dropped, and the measured counts are the ones the committed baseline was
//! recorded from.
//!
//! Three questions; the first and the last are the two every tier-1 corpus
//! test asks, and the middle one is the half of tier 2 no rate reaches:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** Tier 2's deliverable is definitions, structure and
//!    imports, and the rate can only see the imports. The definition census
//!    is therefore asserted exactly on both sides of the store, and beside it
//!    a list of named definitions with their declaration lines — an
//!    owner-frame bug that lost most of the corpus's methods moves no rate,
//!    no bucket and no baseline, so nothing else here would notice it.
//! 3. **The ratchet.** The counts are compared against
//!    `baselines/lua-busted.toml` through the same [`arthron::gate::evaluate`]
//!    the `arthron gate` command uses, so a rate regression — or drift in
//!    either of the two buckets that sit outside the rate — fails the build.
//!
//! Beside the ratchet sits the tally itself, restated. busted is pinned and
//! is never edited, so every number below is a fact about this extractor and
//! this resolver reading a fixed 96 files; a change to any of them is a
//! change in what the track *does*, and must arrive as a deliberate edit here
//! and a deliberate `--rebase` beside it, never as a test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/lua/busted --language lua \
//!     --baseline baselines/lua-busted.toml --rebase --commit <sha>
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
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_lua::extract::{ImportForm, extract};
use arthron::track_lua::project::layout;
use arthron::track_lua::resolve::scan_lua;

const CORPUS: &str = "corpus/lua/busted";
const BASELINE: &str = "baselines/lua-busted.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
///
/// 96 and not the 98 `.lua` files the snapshot holds: `spec/.hidden/` is a
/// hidden directory and the walk does not descend into one. Neither of its
/// two files contains a `require`, so the reference tally is unaffected —
/// but the *file* count is, and stating it here is what keeps a walk that
/// silently started or stopped reading them from passing.
const FILES: usize = 96;
const REFERENCES: u64 = 252;
const LITERAL: u64 = 245;
const DYNAMIC: u64 = 7;

/// Distinct module names the literal sites spell.
///
/// The corpus provenance counted 58 by scanning the text. Parsing agrees
/// exactly, which is the point of restating it: the two methods disagree
/// about the *sites* — a text scan cannot tell `require 'busted'` from
/// `describe('tests require "busted"', ...)`, and six of the corpus's
/// `require` mentions, spread over four files, are inside description
/// strings — and agreeing on the name set is what says the disagreement is
/// the scanner's and not this extractor's.
const DISTINCT_TARGETS: usize = 58;

/// The `build.modules` map the pinned rockspec states.
///
/// 50, not the 51 bracketed entries the file contains: the fifty-first is
/// `['busted'] = 'bin/busted'` under `build.install.bin`, a launcher script
/// rather than a module. Reading it would map `busted` to the wrong file and
/// silently resolve every one of the corpus's ambiguous sites.
const DECLARED_MODULES: usize = 50;

/// Every definition the extractor emits over those 96 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. Definitions
/// are the half of tier 2 the import-rate gate cannot see: an owner-frame bug
/// that lost most of the functions in the corpus would leave every rate,
/// every bucket and the whole ratchet untouched. `Module` counts the 96
/// synthetic chunk nodes, one per file.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 125),
    (DefKind::Method, 117),
    (DefKind::Module, 96),
];

/// Definition nodes the store holds after merging, by kind.
///
/// Equal to [`DEFS`] here, and that equality is itself the assertion: no two
/// definitions in this corpus share an identity, so nothing merged and
/// nothing over-merged. The pair of censuses is the point — the extractor's
/// says nothing was lost on the way in, the store's says nothing was lost or
/// merged on the way through.
///
/// `DefKind::Module` is absent because the driver files a module as a
/// *package* node rather than a definition; those are counted by
/// [`PACKAGES`] instead.
const STORED: &[(DefKind, u64)] = &[(DefKind::Function, 125), (DefKind::Method, 117)];

/// Package nodes: the 96 chunk nodes a `require` names, one per file, and
/// nothing else. A Lua file declares no container for anybody, so the chunk
/// is the only package identity this domain mints.
const PACKAGES: u64 = 96;

/// External nodes: **none, by decision.** A rockspec declares rock names and
/// a rock name is not a module name; this manifest refutes the
/// identification six times out of nine. `External` sits outside both rate
/// terms, so a track that mints none cannot raise its rate by reclassifying.
const EXTERNALS: u64 = 0;

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. Two `statusString` under
/// different chunks cannot both be right unless members are named under the
/// chunk that wrote them, and `block.reject` cannot be right unless a
/// declaration inside a `return function(busted)` factory is kept — which is
/// how nearly all of busted's library is written.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // The two candidates of the ambiguity, both in the graph, which is
    // exactly why the 53 `require 'busted'` sites cannot be resolved: the
    // root shim matches `?.lua` and the package entry matches `?/init.lua`.
    ("$busted", NodeKind::Package, "busted.lua", 1),
    ("$busted/init", NodeKind::Package, "busted/init.lua", 1),
    // A chunk the manifest names, and the one 45 sites require.
    ("$busted/core", NodeKind::Package, "busted/core.lua", 1),
    (
        "$busted/modules/cli",
        NodeKind::Package,
        "busted/modules/cli.lua",
        1,
    ),
    // A chunk no manifest entry names, resolved by `package.path`'s own
    // `?.lua` pattern against the repository root.
    ("$spec/strict", NodeKind::Package, "spec/strict.lua", 1),
    // One file under two module names: `spec.cl_test_module` finds it,
    // `cl_test_module` does not, and the difference is which directory the
    // runner was started in.
    (
        "$spec/cl_test_module",
        NodeKind::Package,
        "spec/cl_test_module.lua",
        1,
    ),
    // A top-level `local function`.
    (
        "$busted/utils#shuffle",
        NodeKind::Definition(DefKind::Function),
        "busted/utils.lua",
        15,
    ),
    // `function fixtures.path(...)` on a chunk-level table.
    (
        "$busted/fixtures#fixtures.path",
        NodeKind::Definition(DefKind::Method),
        "busted/fixtures.lua",
        9,
    ),
    // Declared inside `return function(busted)`, which is how busted's whole
    // library is written. Skipping closures would lose most of this census.
    (
        "$busted/block#block.reject",
        NodeKind::Definition(DefKind::Method),
        "busted/block.lua",
        19,
    ),
    // A three-segment path: `element.env.randomize = function(...)`.
    (
        "$busted/block#element.env.randomize",
        NodeKind::Definition(DefKind::Method),
        "busted/block.lua",
        139,
    ),
    // A member of the table the chunk itself returns — the module's API
    // written as a literal, with no table name to carry.
    (
        "$busted/compatibility#exit",
        NodeKind::Definition(DefKind::Function),
        "busted/compatibility.lua",
        36,
    ),
    // One name, two chunks, two nodes. `local M = {}` binds a local, so
    // nothing about a member's name is visible to another file except
    // through the value its chunk returns.
    (
        "$busted/outputHandlers/plainTerminal#statusString",
        NodeKind::Definition(DefKind::Function),
        "busted/outputHandlers/plainTerminal.lua",
        75,
    ),
    (
        "$busted/outputHandlers/utfTerminal#statusString",
        NodeKind::Definition(DefKind::Function),
        "busted/outputHandlers/utfTerminal.lua",
        114,
    ),
];

#[test]
fn the_lua_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_lua(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Lua.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "lua          resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    for (code, count) in &tally.unresolved {
        println!("             {} {count}", reason_name(*code));
        reasons.insert(reason_name(*code).to_string(), *count);
    }

    // -- phase 0, which decides what a module name is ----------------------

    let cfg = layout(corpus).expect("the corpus has a layout");
    println!(
        "             manifest {:?} modules {}",
        cfg.rockspecs,
        cfg.modules.len()
    );
    assert_eq!(cfg.rockspecs, ["rockspecs/busted-2.3.0-1.rockspec"]);
    assert_eq!(cfg.modules.len(), DECLARED_MODULES);
    assert_eq!(cfg.declared_module("busted.init"), Some("busted/init.lua"));
    assert_eq!(cfg.declared_module("busted.core"), Some("busted/core.lua"));
    // The manifest does not name `busted` — the root shim is not a module it
    // ships — which is why the ambiguity below survives it.
    assert_eq!(cfg.declared_module("busted"), None);
    // `spec/` is not shipped at all, so every spec chunk is reached by the
    // convention alone.
    assert_eq!(cfg.declared_module("spec.strict"), None);

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
    let mut forms: BTreeMap<&str, u64> = BTreeMap::new();
    let mut targets: BTreeSet<String> = BTreeSet::new();
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        let facts = extract(rel, &source);
        re_extracted += facts.refs.len() as u64;
        for r in &facts.refs {
            // The tier-2 contract, checked on real code and not only on a
            // fixture: a call reference here would put references into a
            // denominator this track cannot resolve — and in Lua `require`
            // itself is a call, so this is the line that has to hold.
            assert_eq!(r.kind, RefKind::Import, "{rel}: {}", r.raw_target);
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
        }
        // An import site and its reference are paired by span, so a site with
        // no reference would be a silently dropped import.
        assert_eq!(
            facts.header.imports.len(),
            facts.refs.len(),
            "{rel}: import sites and import references disagree",
        );
        for spec in &facts.header.imports {
            *forms
                .entry(match &spec.form {
                    ImportForm::Module(name) => {
                        targets.insert(name.clone());
                        "literal"
                    }
                    ImportForm::Dynamic => "dynamic",
                })
                .or_default() += 1;
        }
        // Every file declares the chunk a `require` names, first, whether or
        // not it declares anything else.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no chunk",
        );
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
        }
    }
    println!("             forms {forms:?} distinct {}", targets.len());
    println!("             defs  {kinds:?}");

    // -- the definitions, exactly ------------------------------------------

    let want: BTreeMap<u8, u64> = DEFS.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        kinds, want,
        "the definition census moved; tier 2's own deliverable is half \
         definitions and no rate can see them",
    );

    let accounted =
        measured.resolved + measured.external + measured.local_binding + measured.unresolved;
    assert_eq!(
        accounted,
        re_extracted,
        "{re_extracted} references were extracted from {} files but {accounted} were accounted \
         for; a resolver that drops a reference reports a better rate for less work",
        owned.len(),
    );

    // -- the tally, exactly -----------------------------------------------

    assert_eq!(re_extracted, REFERENCES);
    assert_eq!(forms.get("literal").copied(), Some(LITERAL));
    assert_eq!(forms.get("dynamic").copied(), Some(DYNAMIC));
    assert_eq!(targets.len(), DISTINCT_TARGETS);

    assert_eq!(measured.resolved, 99);
    // No rock name is read as a module name, so nothing sits outside the
    // rate's numerator and denominator on that side.
    assert_eq!(measured.external, 0);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. With `external` zero too, every reference this track extracts is
    // in one of the rate's two terms — which is what makes this rate
    // un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    assert_eq!(measured.unresolved, 153);

    // The floor, named. Every literal that names no module under the
    // configured resolution — Penlight, `say`, `luassert`, and the one
    // `cl_test_module` site whose file is right here under a root the
    // repository root is not.
    assert_eq!(reasons.get("ModuleNotFound").copied(), Some(93));
    // `require 'busted'`: `busted.lua` matches `?.lua`, `busted/init.lua`
    // matches `?/init.lua`, and which one wins is decided by `package.path`
    // at run time — which `busted/runner.lua` rewrites from a command-line
    // argument before anything is required.
    assert_eq!(reasons.get("ProjectLayoutUnknown").copied(), Some(53));
    // Five specifiers built by concatenation from a command-line option, and
    // two built from a variable. Never guessed.
    assert_eq!(reasons.get("DynamicModuleSpecifier").copied(), Some(7));
    assert_eq!(
        reasons.len(),
        3,
        "an unexpected reason appeared: {reasons:?}"
    );

    // -- the definitions the store kept, by kind and by name ---------------

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let mut packages = 0u64;
    let mut externals = 0u64;
    read.for_each_node(|_, record| {
        match record {
            NodeRecord::Definition { kind, .. } => *stored.entry(kind).or_default() += 1,
            NodeRecord::Package { .. } => packages += 1,
            NodeRecord::External { .. } => externals += 1,
        }
        Ok(())
    })
    .expect("walking the node table");
    println!("             nodes {stored:?} packages {packages} externals {externals}");
    let want: BTreeMap<u8, u64> = STORED.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "the stored definition census moved");
    assert_eq!(packages, PACKAGES, "the stored package census moved");
    assert_eq!(externals, EXTERNALS, "the stored external census moved");
    // Nothing in this corpus shares an identity with anything else, so the
    // two censuses above are equal and no definition was merged away.
    assert_eq!(
        report.fqn_collisions, 0,
        "two definitions of one identity that the resolver did not call one entity",
    );

    for (fqn, kind, file, line) in PINNED {
        let id = node_id(Domain::Lua, fqn);
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
    drop(read);

    // -- the ratchet ------------------------------------------------------

    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Lua.name(),
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
