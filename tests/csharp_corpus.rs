//! Acceptance for the C# track against the serilog corpus: nothing is
//! dropped, and the measured counts are the ones the committed baseline was
//! recorded from.
//!
//! Four questions, and the middle two are the half of tier 2 no rate reaches:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions, by kind.** Tier 2's deliverable is definitions,
//!    structure and imports, and the rate can only see the imports. The
//!    census is asserted exactly on both sides of the store — an owner-frame
//!    bug that lost most of the corpus's methods moves no rate, no bucket and
//!    no baseline, so nothing else here would notice it.
//! 3. **The definitions, by name.** A census pins the scale; named pins pin
//!    the *shape*. `Serilog.Context#EnricherStack::IEnumerable.GetEnumerator()`
//!    cannot be right unless an explicit interface implementation is kept
//!    apart from the ordinary member beside it, and
//!    `Serilog#Log::Debug(string,object?[]?)` cannot be right unless a
//!    `params` parameter — which this grammar does not wrap in a `parameter`
//!    node — was read.
//! 4. **The ratchet.** The counts are compared against
//!    `baselines/csharp-serilog.toml` through the same
//!    [`arthron::gate::evaluate`] the `arthron gate` command uses, so a rate
//!    regression — or drift in either of the two buckets that sit outside the
//!    rate — fails the build.
//!
//! Beside the ratchet sits the tally itself, restated. serilog is pinned and
//! is never edited, so every number below is a fact about this extractor and
//! this resolver reading a fixed 193 files; a change to any of them is a
//! change in what the track *does*, and must arrive as a deliberate edit here
//! and a deliberate `--rebase` beside it, never as a test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/csharp/serilog --language csharp \
//!     --baseline baselines/csharp-serilog.toml --rebase --commit <sha>
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
use arthron::track_csharp::extract::{ImportForm, extract};
use arthron::track_csharp::resolve::scan_csharp;

mod support;

const CORPUS: &str = "corpus/csharp/serilog";
const BASELINE: &str = "baselines/csharp-serilog.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
const FILES: usize = 193;
const REFERENCES: u64 = 89;
/// `using A.B;` — the plain form, which names a namespace.
const NAMESPACE_FORM: u64 = 85;
/// `global using static Serilog.Events.LogEventLevel;` — the only one.
const STATIC_FORM: u64 = 1;
/// `using File = System.IO.File;` and two more.
const ALIAS_FORM: u64 = 3;
/// `global using`, all of it in three `GlobalUsings.cs` files.
const GLOBAL: u64 = 65;
/// File-level directives, and the files carrying them.
///
/// The provenance's prose says 25 files; the parser says 21. The four it
/// counts and this does not are files whose only `using`-shaped lines are
/// `using var x = …` **statements**, which dispose a resource and import
/// nothing. One of the 21 — `test/Serilog.Tests/Events/LogEventTests.cs` —
/// opens with a UTF-8 BOM before its directive, which is why a line-oriented
/// count misses it and a parser does not.
const FILE_LEVEL: u64 = 24;
const FILES_WITH_FILE_LEVEL: usize = 21;
/// Files carrying no `using` of any kind: what C# 10's `global using` and the
/// SDK's implicit usings look like from the outside.
const FILES_WITH_NO_IMPORT: usize = 169;

/// Every definition the extractor emits over those 193 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. Definitions
/// are the half of tier 2 the import-rate gate cannot see.
///
/// `Module` counts every namespace declaration **and every namespace one
/// implies** — `namespace Serilog.Core.Sinks.Batching;` declares four — plus
/// the global namespace, once per file. All 193 of them: C# has no syntax for
/// declaring the global namespace, so it is not something a file opts into,
/// and every compilation unit begins in it (461 + 193 = 654). It is a count of
/// declarations, not of namespaces; [`PACKAGES`] is the count of namespaces,
/// and it does not move — the 193 merge to the one node they name.
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Function, 0),
    (DefKind::Method, 1203),
    (DefKind::Constructor, 87),
    (DefKind::Type, 240),
    (DefKind::Const, 56),
    (DefKind::Field, 205),
    (DefKind::Property, 146),
    (DefKind::Module, 654),
];

/// Definition nodes the store holds after merging, by kind.
///
/// Lower than [`DEFS`] in exactly two places, and both are C# writing one
/// entity twice:
///
/// - **`Type` 240 → 239.** `partial class PropertyValueConverter` is written
///   in `DepthLimiter.cs` and in `PropertyValueConverter.cs`, and is one type.
/// - **`Method` 1203 → 1202.** `PropertyValueConverter.TryConvertValueTuple`
///   is declared under `#if FEATURE_ITUPLE` and again under `#else` with the
///   same signature, and is one method in every build.
///
/// The pair of censuses is the point — the extractor's says nothing was lost
/// on the way in, the store's says nothing was lost or over-merged on the way
/// through. `DefKind::Module` is absent because the driver files a namespace
/// as a *package* node rather than a definition; those are [`PACKAGES`].
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Method, 1202),
    (DefKind::Constructor, 87),
    (DefKind::Type, 239),
    (DefKind::Const, 56),
    (DefKind::Field, 205),
    (DefKind::Property, 146),
];

/// Package nodes: the 42 namespaces the corpus declares, the three that only
/// a declaration below them implies — `Serilog.Settings`,
/// `Serilog.Tests.Formatting`, `JetBrains` — and the global namespace.
const PACKAGES: u64 = 46;

/// External nodes, one per root namespace segment no file here declares:
/// `System`, `Xunit`, `Newtonsoft`. Named in [`PINNED`], because which
/// assembly supplies a namespace is a claim about the outside world and not a
/// count.
const EXTERNALS: u64 = 3;

/// Every file that declares no type and no member, sorted.
///
/// Five of them declare nothing at all — three `GlobalUsings.cs` and two
/// `AssemblyInfo.cs`, which carry `using` directives and assembly attributes
/// and no declaration. The other two are [`UNREACHED`], and the difference
/// between the two groups is what the assertion below turns from a comment
/// into a check.
const NO_TYPE_OR_MEMBER: &[&str] = &[
    "src/Serilog/Capturing/PropertyBinder.cs",
    "src/Serilog/GlobalUsings.cs",
    "src/Serilog/ILogger.cs",
    "src/Serilog/Properties/AssemblyInfo.cs",
    "test/Serilog.Tests/GlobalUsings.cs",
    "test/Serilog.Tests/Properties/AssemblyInfo.cs",
    "test/TestDummies/GlobalUsings.cs",
];

/// The two files whose types and members the grammar cannot reach, and the
/// declaration each one writes that this track does not see.
///
/// A `#if` that splits a method's *signature* from its body is more than
/// tree-sitter-c-sharp's error recovery can carry past the third occurrence
/// in one type: it collapses the enclosing type declaration into an `ERROR`
/// node, and a member with no enclosing type is not a node. Pinned as a fact
/// rather than left implicit — it is a bound on this track, and the day a
/// grammar upgrade lifts it, this is the assertion that says so.
const UNREACHED: &[(&str, &str)] = &[
    (
        "src/Serilog/Capturing/PropertyBinder.cs",
        "class PropertyBinder",
    ),
    ("src/Serilog/ILogger.cs", "public interface ILogger"),
];

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // A namespace and a type of the same spelling are two identities; `#` is
    // what keeps them apart.
    (
        "Serilog.Events",
        NodeKind::Package,
        "src/Serilog/Events/LogEvent.cs",
        18,
    ),
    (
        "Serilog.Events#LogEventLevel",
        NodeKind::Definition(DefKind::Type),
        "src/Serilog/Events/LogEventLevel.cs",
        20,
    ),
    (
        "Serilog.Events#LogEventLevel::Information",
        NodeKind::Definition(DefKind::Const),
        "src/Serilog/Events/LogEventLevel.cs",
        38,
    ),
    // A namespace no file declares on its own: every one of the four files
    // under `Serilog.Settings.KeyValuePairs` declares it, and a
    // `using Serilog.Settings;` would name it.
    (
        "Serilog.Settings",
        NodeKind::Package,
        "src/Serilog/Settings/KeyValuePairs/KeyValuePairSettings.cs",
        15,
    ),
    // The global namespace: a container with no name, and the one container
    // every file has — including the 188 that declare a namespace of their
    // own, whose `namespace` declaration is itself a member of it.
    ("", NodeKind::Package, "src/Serilog/GlobalUsings.cs", 1),
    // `Guard.cs` declares `namespace JetBrains.Annotations { … }` and then a
    // type *beside* the block, which lands in the global namespace — so one
    // file contributes to two containers.
    (
        "JetBrains.Annotations#NoEnumerationAttribute",
        NodeKind::Definition(DefKind::Type),
        "src/Serilog/Guard.cs",
        5,
    ),
    (
        "#Guard",
        NodeKind::Definition(DefKind::Type),
        "src/Serilog/Guard.cs",
        11,
    ),
    // A generic type carries its arity, the way .NET metadata does.
    (
        "Serilog.Data#LogEventPropertyValueVisitor`2",
        NodeKind::Definition(DefKind::Type),
        "src/Serilog/Data/LogEventPropertyValueVisitor.cs",
        35,
    ),
    // A nested type steps with `+`, and its members with `::`.
    (
        "Serilog.Context#EnricherStack+Enumerator",
        NodeKind::Definition(DefKind::Type),
        "src/Serilog/Context/EnricherStack.cs",
        55,
    ),
    // An explicit interface implementation is not the member of the same name
    // beside it. `EnricherStack` declares three `GetEnumerator`s at once, and
    // a key without the interface would hash all three to one node.
    (
        "Serilog.Context#EnricherStack::GetEnumerator()",
        NodeKind::Definition(DefKind::Method),
        "src/Serilog/Context/EnricherStack.cs",
        39,
    ),
    (
        "Serilog.Context#EnricherStack::IEnumerable.GetEnumerator()",
        NodeKind::Definition(DefKind::Method),
        "src/Serilog/Context/EnricherStack.cs",
        43,
    ),
    // A `params` parameter is one this grammar does not wrap in a `parameter`
    // node. Read only the wrapped ones and this is `Debug(string)`, which
    // `Log.cs:424` already is.
    (
        "Serilog#Log::Debug(string,object?[]?)",
        NodeKind::Definition(DefKind::Method),
        "src/Serilog/Log.cs",
        483,
    ),
    (
        "Serilog#Log::Debug(string)",
        NodeKind::Definition(DefKind::Method),
        "src/Serilog/Log.cs",
        424,
    ),
    // A positional record parameter is a property. C# promotes one for a
    // record and not for a class, and this is the corpus's only record.
    (
        "Serilog.Tests.Support#CollectingFailureListener+LoggingFailure::Sender",
        NodeKind::Definition(DefKind::Property),
        "test/Serilog.Tests/Support/CollectingFailureListener.cs",
        6,
    ),
    // An event is filed the way a property is: what a `+=` names is the
    // add/remove pair, never the backing field.
    (
        "Serilog.Core#LoggingLevelSwitch::MinimumLevelChanged",
        NodeKind::Definition(DefKind::Property),
        "src/Serilog/Core/LoggingLevelSwitch.cs",
        39,
    ),
    // The two `System`s, side by side, which is the whole of why a `using`
    // miss is not a prefix claim: this repository declares the namespace
    // `System` — a polyfill under `#if !NET8_0_OR_GREATER` — while 33 of its
    // imports name `System.*` namespaces only the BCL supplies.
    (
        "System",
        NodeKind::Package,
        "src/Serilog/Util/TimeProvider.cs",
        19,
    ),
    (
        "System",
        NodeKind::External,
        "src/Serilog/Core/Logger.cs",
        15,
    ),
    (
        "Xunit",
        NodeKind::External,
        "test/Serilog.Tests/GlobalUsings.cs",
        30,
    ),
    (
        "Newtonsoft",
        NodeKind::External,
        "test/Serilog.Tests/Formatting/Json/JsonFormatterTests.cs",
        1,
    ),
];

#[test]
fn the_csharp_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        support::missing(corpus);
        return;
    }

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_csharp(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::CSharp.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "csharp       resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
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
    let mut global = 0u64;
    let mut file_level = 0u64;
    let mut files_with_file_level = 0usize;
    let mut files_with_no_import = 0usize;
    let mut unreached: Vec<&str> = Vec::new();
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
        // A directive and its reference are paired by span, so a clause with
        // no reference would be a silently dropped import.
        assert_eq!(
            facts.header.imports.len(),
            facts.refs.len(),
            "{rel}: import clauses and import references disagree",
        );
        let here = facts.header.imports.iter().filter(|i| !i.global).count();
        file_level += here as u64;
        if here > 0 {
            files_with_file_level += 1;
        }
        if facts.header.imports.is_empty() {
            files_with_no_import += 1;
        }
        for spec in &facts.header.imports {
            *forms
                .entry(match spec.form {
                    ImportForm::Namespace(_) => "namespace",
                    ImportForm::Static(_) => "static",
                    ImportForm::Alias { .. } => "alias",
                })
                .or_default() += 1;
            if spec.global {
                global += 1;
            }
        }
        // Every file declares the namespace its definitions live in, first,
        // whether it writes one or lands in the global namespace.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no container",
        );
        if facts.defs.iter().all(|d| d.kind == DefKind::Module) {
            unreached.push(rel.as_str());
        }
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
        }
    }
    println!("             forms {forms:?} global {global} file-level {file_level}");
    println!("             defs  {kinds:?}");

    // -- the definitions, exactly ------------------------------------------

    let want: BTreeMap<u8, u64> = DEFS
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| (k.code(), *n))
        .collect();
    assert_eq!(
        kinds, want,
        "the definition census moved; tier 2's own deliverable is half \
         definitions and no rate can see them",
    );
    // C# declares no free function: every callable is a member of a type.
    assert_eq!(kinds.get(&DefKind::Function.code()), None);

    unreached.sort_unstable();
    assert_eq!(
        unreached, NO_TYPE_OR_MEMBER,
        "the set of files declaring no type and no member moved",
    );
    // Two of those seven are not empty of declarations: they write them and
    // the grammar cannot reach them. Asserted against the source text, so the
    // loss is a measured claim rather than a note.
    for (rel, declared) in UNREACHED {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        assert!(
            source.contains(declared),
            "{rel} no longer writes `{declared}`; the pin is stale",
        );
        assert!(
            NO_TYPE_OR_MEMBER.contains(rel),
            "{rel} declares `{declared}` and the extractor now reaches it —              the grammar bound has lifted, and the censuses above must be re-measured",
        );
    }

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
    assert_eq!(forms.get("namespace").copied(), Some(NAMESPACE_FORM));
    assert_eq!(forms.get("static").copied(), Some(STATIC_FORM));
    assert_eq!(forms.get("alias").copied(), Some(ALIAS_FORM));
    assert_eq!(global, GLOBAL);
    assert_eq!(file_level, FILE_LEVEL);
    assert_eq!(files_with_file_level, FILES_WITH_FILE_LEVEL);
    assert_eq!(files_with_no_import, FILES_WITH_NO_IMPORT);

    // 50 plain directives name a namespace this repository declares, one
    // `using static` and two aliases name a type it declares.
    assert_eq!(measured.resolved, 53);
    // 35 plain directives and one alias name a namespace no file here
    // declares: 33 under `System`, two under `Xunit`, one under `Newtonsoft`.
    // One of the 33 is `ILogger.cs`'s, whose types the grammar could not
    // reach and whose import it read all the same.
    assert_eq!(measured.external, 36);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The bucket that sits outside both rate terms is empty, which is
    // what makes this rate un-gameable by reclassification.
    assert_eq!(measured.local_binding, 0);
    // Every `using` this corpus writes names either a namespace it declares
    // or one another assembly does. A rate of 1.0 is the tightest ratchet
    // there is: one new miss fails the gate.
    assert_eq!(measured.unresolved, 0);
    assert!(
        reasons.is_empty(),
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
    // No identity is shared by two declarations C# does not call one entity.
    // The count the driver returns is the raw store count *minus* what
    // [`arthron::lang::Resolver::mergeable`] accepted, so zero here is the
    // strong claim: every shared identity was one the language writes twice.
    assert_eq!(
        report.fqn_collisions, 0,
        "two declarations shared an identity that C# does not call one entity",
    );
    // What was merged, spelled out. A `partial` type is the one thing in this
    // corpus declared across two *files*, and a full-registry `arthron gate`
    // prints it as `fqn collisions 1` — that line is the raw store count,
    // recomputed by whichever track runs last, and it is documentation rather
    // than a verdict. Pinning the name here is what keeps the two numbers
    // reconcilable instead of merely different.
    let mut across_files: Vec<String> = Vec::new();
    read.for_each_node(|_, record| {
        if let NodeRecord::Definition {
            fqn, declarations, ..
        } = record
        {
            let mut files = declarations.iter().map(|d| d.file.clone());
            if let Some(first) = files.next()
                && files.any(|f| f != first)
            {
                across_files.push(fqn);
            }
        }
        Ok(())
    })
    .expect("walking the node table");
    across_files.sort();
    assert_eq!(
        across_files,
        ["Serilog.Capturing#PropertyValueConverter"],
        "the set of definitions declared across two files moved",
    );

    for (fqn, kind, file, line) in PINNED {
        // An external node's identity carries the `external:` prefix the
        // driver mints it under; a definition's is its FQN as written here.
        let spelled = match kind {
            NodeKind::External => format!("external:{fqn}"),
            _ => (*fqn).to_string(),
        };
        let id = node_id(Domain::CSharp, &spelled);
        let def = definition(&read, &id)
            .unwrap_or_else(|e| panic!("{fqn}: {e}"))
            .unwrap_or_else(|| panic!("{fqn} is not in the store"));
        assert_eq!(def.node.name, *fqn);
        assert_eq!(def.node.kind, *kind, "{fqn}");
        // A namespace is declared by every file under it, so only the sites
        // in the file this pin names are worth printing when it misses.
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
        Lang::CSharp.name(),
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
