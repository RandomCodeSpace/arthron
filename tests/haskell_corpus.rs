//! Acceptance for the Haskell track against the aeson corpus: nothing is
//! dropped, and the measured counts are the ones the committed baseline was
//! recorded from.
//!
//! Four questions. The first and the last are the two every corpus test here
//! asks; the middle two are the halves of a best-effort tier-2 track that no
//! rate reaches:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** Tier 2's deliverable is definitions, structure and
//!    imports, and the rate can only see the imports. The definition census is
//!    therefore asserted exactly on both sides of the store — an owner-frame
//!    bug that lost most of the corpus's constructors moves no rate, no bucket
//!    and no baseline, so nothing else here would notice it. Nine named
//!    definitions with their declaration lines sit beside the census, because
//!    a census pins the scale and only a name pins the shape.
//! 3. **The shortfalls, as numbers.** What this track does *not* read is
//!    pinned too — the 11 import lines the pinned grammar's CPP handling
//!    swallows, and the 8 export-list `module` clauses this contract leaves
//!    out. A recorded under-count that nothing measures is a rumour.
//! 4. **The ratchet.** The counts are compared against
//!    `baselines/haskell-aeson.toml` through the same [`arthron::gate::evaluate`]
//!    the `arthron gate` command uses, so a rate regression — or drift in
//!    either of the two buckets that sit outside the rate — fails the build.
//!
//! Beside the ratchet sits the tally itself, restated. aeson is pinned at
//! `v2.3.1.0` and is never edited, so every number below is a fact about this
//! extractor and this resolver reading a fixed 94 files; a change to any of
//! them is a change in what the track *does*, and must arrive as a deliberate
//! edit here and a deliberate `--rebase` beside it, never as a test that
//! quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/haskell/aeson --language haskell \
//!     --baseline baselines/haskell-aeson.toml --rebase --commit <sha>
//! ```
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken track.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::gate::{Counts, GateVerdict, evaluate, parse_baseline};
use arthron::lang::{FileIndex, Resolver};
use arthron::model::{DefKind, Domain, Lang, RefKind, node_id, reason_name};
use arthron::query::{NodeKind, definition};
use arthron::store::{NodeRecord, ReadStore, Store, StoredOutcome};
use arthron::track_haskell::extract::extract;
use arthron::track_haskell::resolve::{HsResolver, scan_haskell};

const CORPUS: &str = "corpus/haskell/aeson";
const BASELINE: &str = "baselines/haskell-aeson.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
///
/// 94 and not 92: two of the corpus's Haskell paths are git symlinks
/// (`attoparsec-aeson/src/Data/Aeson/Internal/{ByteString,Text}.hs` point at
/// `src/Data/Aeson/Internal/`), the walk resolves a symlink to a file and
/// reads it, and one blob compiled into two packages under two paths really is
/// two modules a GHC invocation can name apart. The provenance names both
/// numbers and says the corpus is where the choice is recorded; this is that
/// record.
const FILES: usize = 94;

/// Import references extracted, and `import`-opening lines in the same bytes.
///
/// The difference is the whole of this track's parse-level shortfall, and it
/// is pinned rather than described: tree-sitter-haskell 0.23.1 swallows
/// `#else` and everything under it into one `cpp` node, so no arm of a CPP
/// conditional after the first is read. [`CPP_DEFICIT`] names every file that
/// pays for it.
const REFERENCES: u64 = 1_074;
const IMPORT_LINES: u64 = 1_085;

/// Where the 11 unread import lines are, file by file.
const CPP_DEFICIT: &[(&str, u64)] = &[
    ("attoparsec-aeson/src/Data/Aeson/Internal/Text.hs", 1),
    ("src/Data/Aeson/Decoding/Text.hs", 1),
    ("src/Data/Aeson/Internal/Text.hs", 1),
    ("src/Data/Aeson/KeyMap.hs", 3),
    ("tests/CastFloat.hs", 5),
];

/// Export-list `module M` clauses, which this contract deliberately does not
/// emit a reference for — `import`-like references here are `import`
/// declarations. Eight of them, in two files. Pinned so the omission stays a
/// measurement rather than a claim.
const EXPORT_MODULE_CLAUSES: u64 = 8;

/// What phase 0 read out of the five `.cabal` manifests.
///
/// Fourteen `hs-source-dirs` entries over eight distinct roots is the fact the
/// corpus was chosen for: `Data.Aeson.Parser.Internal` is at
/// `attoparsec-aeson/src/Data/Aeson/Parser/Internal.hs` and `Data.Aeson` is at
/// `src/Data/Aeson.hs`, same rule, different root, and nothing in a `.hs` file
/// names either. Seven of the fourteen are `aeson-examples`' seven components
/// all naming `src/`.
const MANIFESTS: usize = 5;
const SOURCE_DIR_ENTRIES: usize = 14;
const SOURCE_ROOTS: &[&str] = &[
    "src",
    "tests",
    "attoparsec-aeson/src",
    "attoparsec-iso8601/src",
    "examples/src",
    "text-iso8601/src",
    "text-iso8601/tests",
    "text-iso8601/bench",
];

/// Every definition the extractor emits over those 94 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. Definitions
/// are the half of tier 2 the import-rate gate cannot see: a bug that lost
/// every data constructor in the corpus would leave every rate, every bucket
/// and the whole ratchet untouched. `Module` counts the 94 module nodes, one
/// per file. `Function` counts every *clause* — a signature and each equation
/// under it are separate records here, and merging them is the resolver's job.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 2_178),
    (DefKind::Method, 90),
    (DefKind::Type, 201),
    (DefKind::Constructor, 178),
    (DefKind::Field, 154),
    (DefKind::Module, 94),
];

/// Definition nodes the store holds after merging, by kind.
///
/// Lower than [`DEFS`] where one declaration was written more than once: a
/// type signature, its equations and a `where`-less second clause are one
/// `Function`, and a class method's signature and its default implementation
/// are one `Method`. The pair of censuses is the point — the extractor's says
/// nothing was lost on the way in, the store's says nothing was lost or
/// over-merged on the way through.
///
/// `DefKind::Module` is absent because the driver files a module as a
/// *package* node rather than a definition; those are counted by [`PACKAGES`].
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Function, 1_016),
    (DefKind::Method, 65),
    (DefKind::Type, 201),
    (DefKind::Constructor, 178),
    (DefKind::Field, 154),
];

/// Package nodes: one per file, and no merging, because a module's identity
/// here is its path. Six of them declare the name `Main` and two pairs of them
/// declare one name apiece — see [`MULTI_DECLARED`].
const PACKAGES: u64 = 94;

/// External nodes: one per module-name root segment reaching outside the
/// repository. Named in [`EXTERNAL_NAMES`], because which namespace roots a
/// corpus reaches for is a claim about the outside world and not a count.
const EXTERNALS: u64 = 16;

/// The root segments the corpus's 796 external imports land on.
///
/// Sixteen nodes for some thirty real packages: a Haskell module name states
/// no boundary between the package and the module, and a `.cabal`
/// `build-depends` list names packages without saying which modules they
/// expose. The precision cost is recorded in `track_haskell::resolve`; it
/// moves no reference between the rate's terms.
const EXTERNAL_NAMES: &[&str] = &[
    "Control",
    "Data",
    "Foreign",
    "GHC",
    "Generics",
    "Language",
    "Math",
    "Network",
    "NoThunks",
    "Numeric",
    "Prelude",
    "System",
    "Test",
    "Text",
    "Unsafe",
    "Witherable",
];

/// One module name, several files declaring it: `(module FQN, declared name)`.
///
/// The reason a module's identity here is its file. Six executables declare
/// `module Main` and only their paths tell them apart; two git symlinks put
/// one blob under two paths in two packages, and each package's import binds
/// to its own. None of the eight is a collision, and the report's
/// `fqn_collisions` is asserted at zero below.
const MULTI_DECLARED: &[(&str, &str)] = &[
    ("examples/src/Generic", "Main"),
    ("examples/src/Simplest", "Main"),
    ("examples/src/TemplateHaskell", "Main"),
    ("tests/Tests", "Main"),
    ("text-iso8601/bench/text-iso8601-bench", "Main"),
    ("text-iso8601/tests/text-iso8601-tests", "Main"),
    (
        "attoparsec-aeson/src/Data/Aeson/Internal/ByteString",
        "Data.Aeson.Internal.ByteString",
    ),
    (
        "src/Data/Aeson/Internal/ByteString",
        "Data.Aeson.Internal.ByteString",
    ),
    (
        "attoparsec-aeson/src/Data/Aeson/Internal/Text",
        "Data.Aeson.Internal.Text",
    ),
    ("src/Data/Aeson/Internal/Text", "Data.Aeson.Internal.Text"),
];

/// Which source roots each root's files may bind an import into, by cabal's
/// own rules: the root itself, plus the **library** root of every in-corpus
/// package the owning component's `build-depends` names.
///
/// This resolver does not enforce cross-package visibility — it probes every
/// root in the repository, own root first — and that shortcut can only ever
/// resolve a reference, never launder one outside the denominator. Whether it
/// ever resolves one *cabal would not* is a different question, and this table
/// is what turns the answer into a measurement instead of a claim in a
/// comment. A test-suite or benchmark root is nobody else's dependency, so
/// `aeson`'s `tests` is reachable from `tests` alone.
const VISIBLE: &[(&str, &[&str])] = &[
    // aeson's library depends on `text-iso8601`, which is in the corpus.
    ("src", &["src", "text-iso8601/src"]),
    // aeson's test-suite depends on `aeson`.
    ("tests", &["tests", "src"]),
    ("attoparsec-aeson/src", &["attoparsec-aeson/src", "src"]),
    ("attoparsec-iso8601/src", &["attoparsec-iso8601/src"]),
    ("examples/src", &["examples/src", "src"]),
    ("text-iso8601/src", &["text-iso8601/src"]),
    (
        "text-iso8601/tests",
        &["text-iso8601/tests", "text-iso8601/src"],
    ),
    (
        "text-iso8601/bench",
        &[
            "text-iso8601/bench",
            "text-iso8601/src",
            "attoparsec-iso8601/src",
        ],
    ),
];

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape*. `…Key#Key` and
/// `…Key#Key.Key` cannot both be right unless Haskell's two namespaces were
/// kept apart; `…Internal#Object` and `…Internal#Value.Object` cannot both be
/// right unless a data constructor was filed under its type rather than beside
/// it; and `…FromJSON#.:` cannot be right unless an operator lost its
/// parentheses and a reserved `:` inside a member name cost nothing.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // The module an `import Data.Aeson` resolves to, named by its file.
    ("src/Data/Aeson", NodeKind::Package, "src/Data/Aeson.hs", 15),
    // One of the six `module Main` files, at an identity of its own.
    ("tests/Tests", NodeKind::Package, "tests/Tests.hs", 3),
    // A newtype and its constructor: one word, two namespaces, two nodes.
    (
        "src/Data/Aeson/Key#Key",
        NodeKind::Definition(DefKind::Type),
        "src/Data/Aeson/Key.hs",
        43,
    ),
    (
        "src/Data/Aeson/Key#Key.Key",
        NodeKind::Definition(DefKind::Constructor),
        "src/Data/Aeson/Key.hs",
        43,
    ),
    (
        "src/Data/Aeson/Key#Key.unKey",
        NodeKind::Definition(DefKind::Field),
        "src/Data/Aeson/Key.hs",
        43,
    ),
    // `type Object = KeyMap Value` and the `Object` constructor of `Value`,
    // declared six lines apart in one module and sharing one word.
    (
        "src/Data/Aeson/Types/Internal#Object",
        NodeKind::Definition(DefKind::Type),
        "src/Data/Aeson/Types/Internal.hs",
        360,
    ),
    (
        "src/Data/Aeson/Types/Internal#Value.Object",
        NodeKind::Definition(DefKind::Constructor),
        "src/Data/Aeson/Types/Internal.hs",
        366,
    ),
    // A class and a method of it, whose signature and default implementation
    // are one node declared at two lines.
    (
        "src/Data/Aeson/Types/ToJSON#ToJSON",
        NodeKind::Definition(DefKind::Type),
        "src/Data/Aeson/Types/ToJSON.hs",
        292,
    ),
    (
        "src/Data/Aeson/Types/ToJSON#ToJSON.toJSON",
        NodeKind::Definition(DefKind::Method),
        "src/Data/Aeson/Types/ToJSON.hs",
        294,
    ),
    // An operator, named without its parentheses, whose FQN carries the `:`
    // the house grammar reserves — harmlessly, because it is not at the front.
    (
        "src/Data/Aeson/Types/FromJSON#.:",
        NodeKind::Definition(DefKind::Function),
        "src/Data/Aeson/Types/FromJSON.hs",
        857,
    ),
    // A record field of a multi-field constructor, filed under its type.
    (
        "src/Data/Aeson/Types/Internal#Options.fieldLabelModifier",
        NodeKind::Definition(DefKind::Field),
        "src/Data/Aeson/Types/Internal.hs",
        710,
    ),
    // The external node 71 files reach: every `Data.*` import that leaves the
    // repository, on the coarsest unit nameable without guessing.
    (
        "Data",
        NodeKind::External,
        "src/Data/Aeson/Types/Internal.hs",
        89,
    ),
];

#[test]
fn the_haskell_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {CORPUS} — see README");
        return;
    }

    // -- phase 0, read straight off the manifests --------------------------

    let cfg = HsResolver
        .config(corpus, &FileIndex { files: Vec::new() })
        .expect("the corpus has a layout");
    println!("haskell      roots {:?}", cfg.source_roots);
    assert_eq!(cfg.manifests.len(), MANIFESTS, "{:?}", cfg.manifests);
    assert_eq!(cfg.source_dir_entries, SOURCE_DIR_ENTRIES);
    assert_eq!(cfg.source_roots, SOURCE_ROOTS);
    // Five manifests, five package names — and every one of them is depended
    // on by something, which is what makes `build-depends` say the repository
    // links against code it does not contain.
    assert_eq!(cfg.packages.len(), MANIFESTS);
    assert!(
        cfg.declares_outside_dependency(),
        "aeson depends on base; the external gate must be open",
    );

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_haskell(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Haskell.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "haskell      resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
        measured.resolved, measured.external, measured.local_binding, measured.unresolved,
    );
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    for (code, count) in &tally.unresolved {
        println!("             {} {count}", reason_name(*code));
        reasons.insert(reason_name(*code).to_string(), *count);
    }
    // Two files declaring one module name is not a collision here: the
    // identity is the path, so `module Main` written six times is six nodes.
    assert_eq!(report.fqn_collisions, 0);
    assert!(report.file_errors.is_empty(), "{:?}", report.file_errors);

    // -- completeness -----------------------------------------------------

    let store = Store::open(&db).expect("store opens");
    let owned = store.known_files().expect("known files");
    let package_names = store.package_names().expect("package names");
    drop(store);
    assert_eq!(owned.len(), FILES, "the scan owned a different file set");

    let mut re_extracted = 0u64;
    let mut import_lines = 0u64;
    let mut export_module_clauses = 0u64;
    let mut deficit: BTreeMap<String, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        let facts = extract(rel, &source);
        re_extracted += facts.refs.len() as u64;
        for r in &facts.refs {
            // The tier-2 contract, checked on real code and not only on a
            // fixture: a call or type reference here would put sites into a
            // denominator this track cannot resolve.
            assert_eq!(r.kind, RefKind::Import, "{rel}: {}", r.raw_target);
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
        }
        // An import declaration and its reference are paired by span, so a
        // declaration with no reference would be a silently dropped import.
        assert_eq!(
            facts.header.imports.len(),
            facts.refs.len(),
            "{rel}: import declarations and import references disagree",
        );
        // Every file is a module whether or not it declares anything, and the
        // driver reads that first record as the container this file's edges
        // start at.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no module",
        );
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
        }

        // An independent, parser-free count of what the bytes contain, so the
        // shortfall below is measured rather than described.
        let mut here = 0u64;
        for line in source.lines() {
            if line.starts_with("import ") || line == "import" {
                here += 1;
            }
            let trimmed = line.trim_start();
            if !line.starts_with("module ")
                && (trimmed.starts_with("module ")
                    || trimmed.starts_with(", module ")
                    || trimmed.starts_with("( module "))
            {
                export_module_clauses += 1;
            }
        }
        import_lines += here;
        if here != facts.refs.len() as u64 {
            deficit.insert(rel.clone(), here - facts.refs.len() as u64);
        }
    }
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

    // -- the shortfalls, as numbers ----------------------------------------

    assert_eq!(re_extracted, REFERENCES);
    assert_eq!(import_lines, IMPORT_LINES);
    let want: BTreeMap<String, u64> = CPP_DEFICIT
        .iter()
        .map(|(f, n)| ((*f).to_string(), *n))
        .collect();
    assert_eq!(
        deficit, want,
        "the CPP shortfall moved: every unread import line must be inside an \
         `#else`/`#elif` arm the pinned grammar swallows",
    );
    assert_eq!(
        import_lines - re_extracted,
        11,
        "11 import lines, in five files, out of {IMPORT_LINES}",
    );
    assert_eq!(export_module_clauses, EXPORT_MODULE_CLAUSES);

    // -- the tally, exactly -----------------------------------------------

    // Every import naming a module under one of the eight declared source
    // roots, and there is no other kind of hit: a Haskell import names a
    // module and nothing finer.
    assert_eq!(measured.resolved, 278);
    // Every import naming a module no file under any declared root provides.
    // Not an inference: a home module *is* a file under a declared root, and
    // this scan enumerates both sides.
    assert_eq!(measured.external, 796);
    // Tier 2 emits no expression-level reference, so nothing can name a local.
    // The bucket that sits outside both rate terms is empty, which is what
    // makes this rate un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    // Nothing in between. In particular `ProjectLayoutUnknown` is zero, which
    // is the anti-laundering guard reporting that the root map explains every
    // module the walk found — had a component's `hs-source-dirs` gone unread,
    // its modules would land here rather than outside the denominator.
    assert_eq!(measured.unresolved, 0);
    assert!(
        reasons.is_empty(),
        "an unexpected reason appeared: {reasons:?}"
    );

    // -- the definitions the store kept, by kind and by name ---------------

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let mut packages = 0u64;
    let mut externals: Vec<String> = Vec::new();
    read.for_each_node(|_, record| {
        match record {
            NodeRecord::Definition { kind, .. } => *stored.entry(kind).or_default() += 1,
            NodeRecord::Package { .. } => packages += 1,
            NodeRecord::External { package, .. } => externals.push(package),
        }
        Ok(())
    })
    .expect("walking the node table");
    externals.sort();
    println!("             nodes {stored:?} packages {packages} externals {externals:?}");
    let want: BTreeMap<u8, u64> = STORED.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "the stored definition census moved");
    assert_eq!(packages, PACKAGES, "the stored package census moved");
    assert_eq!(externals.len() as u64, EXTERNALS);
    assert_eq!(externals, EXTERNAL_NAMES);

    // One declared name, several files: the reason a module's identity is its
    // path, asserted on the corpus rather than argued in a comment.
    for (fqn, declared) in MULTI_DECLARED {
        assert_eq!(
            package_names.get(*fqn).map(String::as_str),
            Some(*declared),
            "{fqn} does not declare {declared}",
        );
    }
    assert_eq!(package_names.len() as u64, PACKAGES);

    for (fqn, kind, file, line) in PINNED {
        // An external node's identity carries the `external:` prefix the
        // driver mints it under; a definition's is its FQN as written here.
        let spelled = match kind {
            NodeKind::External => format!("external:{fqn}"),
            _ => (*fqn).to_string(),
        };
        let id = node_id(Domain::Haskell, &spelled);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("{fqn} is not in the store"));
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
    // -- every edge is one cabal would also draw ---------------------------

    // The resolver probes every root in the repository; cabal lets a component
    // reach only its own roots and its dependencies' libraries. The shortcut
    // can only resolve a reference, never launder one — but a resolution cabal
    // would refuse is a wrong edge, and a wrong edge beats a miss only in the
    // sense that it is worse. Measured here rather than asserted in a comment.
    let mut modules: BTreeMap<arthron::model::NodeId, String> = BTreeMap::new();
    read.for_each_node(|id, record| {
        if let NodeRecord::Package { import_path, .. } = record {
            modules.insert(id, import_path);
        }
        Ok(())
    })
    .expect("walking the node table");
    let visible: BTreeMap<&str, &[&str]> = VISIBLE.iter().copied().collect();
    let root_of = |path: &str| -> &'static str {
        SOURCE_ROOTS
            .iter()
            .copied()
            .filter(|root| path.strip_prefix(root).is_some_and(|r| r.starts_with('/')))
            .max_by_key(|root| root.len())
            .unwrap_or_else(|| panic!("{path} sits under no declared source root"))
    };
    let mut edges = 0u64;
    read.for_each_row(|key, record| {
        let StoredOutcome::Resolved(id) = record.outcome else {
            return Ok(());
        };
        let target = modules
            .get(&id)
            .unwrap_or_else(|| panic!("{}: resolved to a node that is not a module", key.file));
        let from = root_of(&key.file);
        let into = root_of(&format!("{target}.hs"));
        edges += u64::from(record.count);
        assert!(
            visible[from].contains(&into),
            "{}: `import {}` binds into {into}, which {from} does not depend on",
            key.file,
            key.raw_target,
        );
        Ok(())
    })
    .expect("walking the reference rows");
    assert_eq!(
        edges, measured.resolved,
        "every resolved reference must be one of these rows",
    );

    drop(read);

    // -- the ratchet ------------------------------------------------------

    let text =
        std::fs::read_to_string(BASELINE).unwrap_or_else(|e| panic!("reading {BASELINE}: {e}"));
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Haskell.name(),
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
