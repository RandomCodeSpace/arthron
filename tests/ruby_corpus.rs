//! Acceptance for the Ruby track against the rack corpus: nothing is dropped,
//! and the measured counts are the ones the committed baseline was recorded
//! from.
//!
//! Three questions; the first and the last are the two every tier-1 corpus
//! test asks, and the middle one is the half of tier 2 no rate reaches:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** Tier 2's deliverable is definitions, structure
//!    and imports, and the rate can only see the imports. The definition
//!    census is therefore asserted exactly on both sides of the store — an
//!    owner-frame bug that lost most of the corpus's methods moves no rate,
//!    no bucket and no baseline, so nothing else here would notice it.
//! 3. **The ratchet.** The counts are compared against
//!    `baselines/ruby-rack.toml` through the same [`arthron::gate::evaluate`]
//!    the `arthron gate` command uses, so a rate regression — or drift in
//!    either of the two buckets that sit outside the rate — fails the build.
//!
//! Beside the ratchet sits the tally itself, restated. rack is pinned and is
//! never edited, so every number below is a fact about this extractor and
//! this resolver reading a fixed 93 files; a change to any of them is a
//! change in what the track *does*, and must arrive as a deliberate edit here
//! and a deliberate `--rebase` beside it, never as a test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/ruby/rack --language ruby \
//!     --baseline baselines/ruby-rack.toml --rebase --commit <sha>
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken track.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::model::{DefKind, Domain, Lang, RefKind, node_id, reason_name};
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_ruby::extract::{ImportForm, extract};
use arthron::track_ruby::resolve::scan_ruby;

const CORPUS: &str = "corpus/ruby/rack";
const BASELINE: &str = "baselines/ruby-rack.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
const FILES: usize = 93;
const REFERENCES: u64 = 342;
const REQUIRE_RELATIVE: u64 = 247;
const LOAD_PATH: u64 = 94;
const DYNAMIC: u64 = 1;

/// Every definition the extractor emits over those 93 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. Definitions
/// are the half of tier 2 the import-rate gate cannot see: an owner-frame bug
/// that lost most of the methods in the corpus would leave every rate, every
/// bucket and the whole ratchet untouched. `Module` counts the 93 synthetic
/// feature nodes as well as the modules the source writes.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 2),
    (DefKind::Method, 633),
    (DefKind::Type, 88),
    (DefKind::Const, 156),
    (DefKind::Property, 44),
    (DefKind::Module, 161),
];

/// Definition nodes the store holds after merging, by kind.
///
/// Lower than [`DEFS`] where reopening merges: `def call` written at the top
/// level of two files is one `Object#call`. The pair of censuses is the
/// point — the extractor's says nothing was lost on the way in, the store's
/// says nothing was lost or over-merged on the way through.
///
/// `DefKind::Module` is absent because the driver files a module as a
/// *package* node rather than a definition; those are counted by
/// [`PACKAGES`] instead.
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Function, 1),
    (DefKind::Method, 630),
    (DefKind::Type, 88),
    (DefKind::Const, 156),
    (DefKind::Property, 44),
];

/// Package nodes: the 93 feature nodes a `require` names, plus the modules
/// the source declares once reopening has merged them — 161 module
/// definitions in, 106 identities out, which is what `module Rack` written in
/// most of a gem's files looks like from the other side.
const PACKAGES: u64 = 106;

/// External nodes: the one gem `rack.gemspec` declares and `test/helper.rb`
/// requires. Named in [`PINNED`], because which gem ships a require path is
/// a claim about the outside world and not a count.
const EXTERNALS: u64 = 1;

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. Two `params` under
/// different owners cannot both be right unless the owner frames were walked
/// to the bottom, and `Rack::Builder.parse_file` cannot be right unless a
/// singleton method is separated from an instance one of the same name.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // The feature `require 'rack/version'` names, and the module the same
    // file declares: one file, two identities, and the path-derived one is
    // what an import resolves to.
    (
        "$lib/rack/version",
        NodeKind::Package,
        "lib/rack/version.rb",
        1,
    ),
    ("Rack", NodeKind::Package, "lib/rack.rb", 17),
    (
        "Rack::VERSION",
        NodeKind::Definition(DefKind::Const),
        "lib/rack/version.rb",
        9,
    ),
    (
        "Rack.release",
        NodeKind::Definition(DefKind::Method),
        "lib/rack/version.rb",
        14,
    ),
    (
        "Rack::Request",
        NodeKind::Definition(DefKind::Type),
        "lib/rack/request.rb",
        16,
    ),
    (
        "Rack::Request#params",
        NodeKind::Definition(DefKind::Method),
        "lib/rack/request.rb",
        72,
    ),
    (
        "Rack::Request::Helpers#params",
        NodeKind::Definition(DefKind::Method),
        "lib/rack/request.rb",
        556,
    ),
    (
        "Rack::Builder.parse_file",
        NodeKind::Definition(DefKind::Method),
        "lib/rack/builder.rb",
        65,
    ),
    // `require 'minitest/global_expectations/autorun'`. That path is shipped
    // by `minitest-global_expectations`, and `minitest` — also declared, also
    // a prefix — does not ship it. Both answers classify the reference the
    // same way and sit outside both rate terms; only one names the right
    // package on the node the reference points at.
    (
        "minitest-global_expectations",
        NodeKind::External,
        "test/helper.rb",
        26,
    ),
];

#[test]
fn the_ruby_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_ruby(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Ruby.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "ruby         resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
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
    let mut forms: BTreeMap<&str, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        let facts = extract(rel, &source);
        re_extracted += facts.refs.len() as u64;
        for r in &facts.refs {
            // The tier-2 contract, checked on real code and not only on a
            // fixture: a call or type reference here would put references
            // into a denominator this track cannot resolve.
            assert_eq!(r.kind, RefKind::Import, "{rel}: {}", r.raw_target);
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
        }
        // An import clause and its reference are paired by span, so a clause
        // with no reference would be a silently dropped import.
        assert_eq!(
            facts.header.imports.len(),
            facts.refs.len(),
            "{rel}: import clauses and import references disagree",
        );
        for spec in &facts.header.imports {
            *forms
                .entry(match spec.form {
                    ImportForm::Relative(_) => "relative",
                    ImportForm::LoadPath(_) => "load-path",
                    ImportForm::Dynamic => "dynamic",
                })
                .or_default() += 1;
        }
        // Every file declares the feature a `require` names, first, whether
        // or not it declares a constant.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no feature",
        );
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
        }
    }
    println!("             forms {forms:?}");
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
    assert_eq!(forms.get("relative").copied(), Some(REQUIRE_RELATIVE));
    assert_eq!(forms.get("load-path").copied(), Some(LOAD_PATH));
    assert_eq!(forms.get("dynamic").copied(), Some(DYNAMIC));

    // Every `require_relative` in rack names a real sibling, and every one of
    // the 43 `autoload`s plus the single `require 'rack'` reaches `lib/`.
    assert_eq!(measured.resolved, 291);
    // The one gem the source requires by a name `rack.gemspec` declares.
    assert_eq!(measured.external, 1);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The bucket that sits outside both rate terms is empty, which is
    // what makes this rate un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    assert_eq!(measured.unresolved, 50);

    // The floor, named. Ruby's standard library is not indexed here, so
    // `require 'time'` is a package outside the repository that was not
    // indexed — and it counts *against* the rate rather than being waved
    // through as external.
    assert_eq!(reasons.get("UnknownPackage").copied(), Some(49));
    // `Rack::Builder.parse_file` ends in `require path`. A specifier built at
    // runtime is never guessed.
    assert_eq!(reasons.get("DynamicModuleSpecifier").copied(), Some(1));
    assert_eq!(
        reasons.len(),
        2,
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

    for (fqn, kind, file, line) in PINNED {
        // An external node's identity carries the `external:` prefix the
        // driver mints it under; a definition's is its FQN as written here.
        let spelled = match kind {
            NodeKind::External => format!("external:{fqn}"),
            _ => (*fqn).to_string(),
        };
        let id = node_id(Domain::Ruby, &spelled);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("{fqn} is not in the store"));
        assert_eq!(def.node.name, *fqn);
        assert_eq!(def.node.kind, *kind, "{fqn}");
        // `module Rack` is reopened in most of the gem, so only the sites in
        // the file this pin names are worth printing when it misses.
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
        Lang::Ruby.name(),
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
