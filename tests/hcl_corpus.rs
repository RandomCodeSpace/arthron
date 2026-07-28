//! Acceptance for the HCL track against the terraform-aws-vpc corpus:
//! nothing is dropped, the tier-2 contract holds on real code, and the
//! measured counts are the ones the committed baseline was recorded from.
//!
//! HCL is a **tier-2, best-effort** language, so what this file gates is an
//! **import-resolution rate** — `Resolved / (Resolved + Unresolved)` over the
//! import-like references the extractor emits, and nothing else. It is not
//! comparable with Go's or Java's rate, and it is never aggregated with
//! either.
//!
//! # Read the denominator before the rate
//!
//! **24 references over 65 files and 1,912 definitions.** HCL has no import
//! statement: the only site in a `.tf` file that names something declared
//! elsewhere is a `module` block's `source`, and this corpus writes 24 of
//! them. Everything else Terraform writes — 750 `var.<name>`, 191 `local.`,
//! 1,188 `module.<name>.<output>` and every `<type>.<name>` resource address
//! — is expression-level and out of tier-2 scope, so none of it is emitted
//! and none of it enters this denominator. A high rate over 24 references is
//! a small claim honestly made, not a large one; the definition census below
//! is the half of tier 2 the rate cannot see, and it is what most of this
//! file asserts.
//!
//! Five questions, because a rate this small is only worth reading if you can
//! answer all of them:
//!
//! 1. **Completeness.** Every reference the extractor emits ends in exactly
//!    one of `Resolved`, `External` or `Unresolved(reason)`. The check
//!    re-extracts the same files independently and compares totals, because a
//!    resolver that silently dropped its hardest references would otherwise
//!    report a *better* rate for doing less work.
//! 2. **The definitions.** Asserted exactly on both sides of the store, by
//!    kind, by Terraform block type, and by name with declaration lines. An
//!    extractor bug that lost most of the corpus's 1,295 outputs would move
//!    no rate, no bucket and no baseline, so nothing else here would notice.
//! 3. **The directory really is the unit.** `modules/flow-log` and
//!    `examples/flow-log` share a basename, and the five `module` blocks in
//!    the second name the first. The edges into both are asserted by name:
//!    a resolver keyed on the last path segment binds the caller to itself,
//!    which is a wrong edge, and every tally in this file would still agree.
//! 4. **The one external is the one the corpus writes.** `External` sits
//!    outside both terms of the rate, so the count is pinned and the node is
//!    named — the registry address at `examples/flow-log/main.tf:101`.
//! 5. **The ratchet.** The counts are compared against
//!    `baselines/hcl-terraform-aws-vpc.toml` through the same
//!    [`arthron::gate::evaluate`] the `arthron gate` command uses, so a rate
//!    regression — or drift in either of the two buckets that sit outside the
//!    rate, or a shrinking denominator — fails the build.
//!
//! terraform-aws-vpc is pinned and is never edited, so every number below is
//! a fact about this extractor and this resolver reading a fixed 65 files; a
//! change to any of them is a change in what the track *does*, and must
//! arrive as a deliberate edit here and a deliberate `--rebase` beside it,
//! never as a test that quietly moved.
//!
//! Re-base with the product's own command:
//!
//! ```text
//! arthron gate corpus/hcl/terraform-aws-vpc --language hcl \
//!     --baseline baselines/hcl-terraform-aws-vpc.toml --rebase --commit 3ffbd46
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
use arthron::store::{NodeRecord, ReadStore, Store};
use arthron::track_hcl::extract::extract;
use arthron::track_hcl::lang::HclLang;
use arthron::track_hcl::resolve::scan_hcl;

mod support;

const CORPUS: &str = "corpus/hcl/terraform-aws-vpc";
const BASELINE: &str = "baselines/hcl-terraform-aws-vpc.toml";

/// The measurement this baseline was recorded from, restated. See the module
/// header for why these are exact and not bounds.
const FILES: usize = 65;
const REFERENCES: u64 = 24;

/// Thirteen of the 65 files are **zero bytes** — every example directory
/// carries a `variables.tf` whether or not it declares anything. A zero-byte
/// source file is a real input a scanner has to survive, and it still
/// declares its directory: see [`PINNED`], where one of them is the
/// declaration site of its container.
const EMPTY_FILES: usize = 13;

/// Every `module` source by the form it was written in. All 24 are plain
/// string literals, which is what Terraform requires — a `source` may not be
/// interpolated or computed. The `dynamic` arm of the extractor is therefore
/// **unexercised by this corpus** and is held by fixtures alone
/// (`tests/hcl_extract.rs`, `tests/hcl_resolve.rs`); it is implemented
/// because a site this build cannot read is still a site, and dropping it
/// would be the one thing the never-drop contract forbids.
const FORMS: &[(&str, u64)] = &[("literal", 24)];

/// Every definition the extractor emits over those 65 files, by kind.
///
/// Asserted exactly, for the same reason the reference tally is. Definitions
/// are the half of tier 2 the import-rate gate cannot see, and with a
/// denominator of 24 that half is almost all of what this track produces.
///
/// `Module` is 65 and is the *container*: every `.tf` file declares the
/// directory it sits in, which is what a Terraform module is. The 65 collapse
/// into the 16 package nodes of [`PACKAGES`].
const DEFS: &[(DefKind, u64)] = &[
    (DefKind::Const, 121),
    (DefKind::Field, 1295),
    (DefKind::Module, 65),
    (DefKind::Var, 431),
];

/// The same definitions by the **Terraform block type** that wrote them,
/// which is the census a reader of a `.tf` file can check by eye.
///
/// The kind census above cannot: four different block types are
/// [`DefKind::Var`], because a small kind vocabulary is deliberate and HCL
/// declares four things a reference does exactly one thing with. The address
/// prefix is what keeps them apart in the FQN, and this is that count.
const BLOCKS: &[(&str, u64)] = &[
    // The file's own directory: one per file, 65 files.
    ("<container>", 65),
    ("data", 26),
    // One per attribute of a `locals` block, across 34 such blocks.
    ("local", 121),
    ("module", 24),
    ("output", 1295),
    ("resource", 96),
    ("var", 285),
];

/// Definition nodes the store holds, by kind.
///
/// Equal to [`DEFS`] but for the containers, which are package nodes and
/// counted by [`PACKAGES`]. Nothing merges: Terraform rejects two
/// declarations of one address in one module, so
/// [`arthron::lang::Resolver::mergeable`] answers `false` here and a shared
/// identity would be a genuine collision — [`COLLISIONS`] is zero and must
/// stay zero.
const STORED: &[(DefKind, u64)] = &[
    (DefKind::Const, 121),
    (DefKind::Field, 1295),
    (DefKind::Var, 431),
];

/// Package nodes: one per directory holding at least one `.tf` file.
///
/// The root module, two child modules, and thirteen example configurations.
/// Every one of them is declared by *several* files and that is not a
/// collision — being written by every file under it is what a Terraform
/// module is, exactly as a Go package is.
const PACKAGES: u64 = 16;

/// External nodes. **One**, and it is the one the corpus provenance names:
/// the public registry address at `examples/flow-log/main.tf:101`. Every
/// other `source` in the corpus is a relative path into this repository.
const EXTERNALS: u64 = 1;

/// Distinct FQNs a definition in more than one file claims. **Zero.**
/// Terraform rejects two declarations of one address in one module, so a
/// non-zero count here is a bug in this track or a corpus that does not
/// `terraform validate`.
const COLLISIONS: u64 = 0;

/// Named nodes, spelled out: `(fqn, kind, declaring file, line)`.
///
/// A census pins the scale; these pin the *shape* — that a container is its
/// whole path, that the six address spaces stay apart, and that a zero-byte
/// file still declares the directory it sits in.
const PINNED: &[(&str, NodeKind, &str, u32)] = &[
    // The root module, declared by all five of its files. A container is a
    // package node: several files declaring it is what it is for.
    ("//", NodeKind::Package, "main.tf", 1),
    // A zero-byte file is a file. `examples/simple/variables.tf` is empty and
    // is one of the four declaration sites of its directory.
    (
        "//examples/simple",
        NodeKind::Package,
        "examples/simple/variables.tf",
        1,
    ),
    // The two directories that share a basename, both present, both their
    // whole path.
    (
        "//modules/flow-log",
        NodeKind::Package,
        "modules/flow-log/main.tf",
        1,
    ),
    (
        "//examples/flow-log",
        NodeKind::Package,
        "examples/flow-log/main.tf",
        1,
    ),
    // One of each address space, so that a grammar change cannot quietly
    // merge two of them.
    (
        "//#resource.aws_vpc.this",
        NodeKind::Definition(DefKind::Var),
        "main.tf",
        28,
    ),
    (
        "//#data.aws_region.current",
        NodeKind::Definition(DefKind::Var),
        "vpc-flow-logs.tf",
        1,
    ),
    (
        "//#var.cidr",
        NodeKind::Definition(DefKind::Var),
        "variables.tf",
        29,
    ),
    (
        "//#output.vpc_id",
        NodeKind::Definition(DefKind::Field),
        "outputs.tf",
        11,
    ),
    (
        "//examples/simple#module.vpc",
        NodeKind::Definition(DefKind::Var),
        "examples/simple/main.tf",
        25,
    ),
    // A local value: declared by an attribute of a `locals` block, which has
    // no label of its own.
    (
        "//examples/simple#local.name",
        NodeKind::Definition(DefKind::Const),
        "examples/simple/main.tf",
        8,
    ),
    // The same simple name in two directories is two nodes, because the
    // container is the whole path.
    (
        "//modules/flow-log#var.create",
        NodeKind::Definition(DefKind::Var),
        "modules/flow-log/variables.tf",
        1,
    ),
    // The module call whose source is the one external address.
    (
        "//examples/flow-log#module.s3_bucket",
        NodeKind::Definition(DefKind::Var),
        "examples/flow-log/main.tf",
        101,
    ),
];

/// Every resolved edge, by the container it lands in and the module calls
/// that make it. All 23 of them, named — which is what proves the directory
/// is the unit of resolution and that no call bound to the wrong one.
///
/// `examples/flow-log` is in this list with **no** incoming edge, and that is
/// the assertion the corpus was chosen for: its five `module` blocks name
/// `../../modules/flow-log`, and a resolver keyed on the last path segment
/// would bind every one of them to the directory they were written in.
const EDGES: &[(&str, &[&str])] = &[
    (
        "//",
        &[
            "//examples/block-public-access#module.vpc",
            "//examples/complete#module.vpc",
            "//examples/flow-log#module.vpc",
            "//examples/ipam#module.vpc_ipam_set_cidr",
            "//examples/ipam#module.vpc_ipam_set_netmask",
            "//examples/ipv6-dualstack#module.vpc",
            "//examples/ipv6-only#module.vpc",
            "//examples/issues#module.vpc_issue_108",
            "//examples/issues#module.vpc_issue_44",
            "//examples/issues#module.vpc_issue_46",
            "//examples/manage-default-vpc#module.vpc",
            "//examples/network-acls#module.vpc",
            "//examples/outpost#module.vpc",
            "//examples/secondary-cidr-blocks#module.vpc",
            "//examples/separate-route-tables#module.vpc",
            "//examples/simple#module.vpc",
        ],
    ),
    (
        "//modules/flow-log",
        &[
            "//examples/flow-log#module.disabled",
            "//examples/flow-log#module.flow_log",
            "//examples/flow-log#module.flow_log_cloudwatch_external",
            "//examples/flow-log#module.flow_log_s3",
            "//examples/flow-log#module.flow_log_s3_parquet",
        ],
    ),
    (
        "//modules/vpc-endpoints",
        &[
            "//examples/complete#module.vpc_endpoints",
            "//examples/complete#module.vpc_endpoints_nocreate",
        ],
    ),
    ("//examples/flow-log", &[]),
    ("//examples/simple", &[]),
];

#[test]
fn the_hcl_track_drops_nothing_and_holds_its_baseline() {
    let corpus = Path::new(CORPUS);
    if !corpus.is_dir() {
        support::missing(corpus);
        return;
    }
    let walked = source_files::<HclLang>(corpus).expect("walking the corpus");
    assert_eq!(walked.len(), FILES, "the walk found a different file set");

    let scratch = tempfile::tempdir().expect("scratch dir");
    let db = scratch.path().join("graph.redb");
    let report = scan_hcl(corpus, &db).expect("the corpus scans");
    let tally = report
        .per_lang
        .get(&Lang::Hcl.code())
        .cloned()
        .unwrap_or_default();

    let measured = Counts {
        resolved: tally.resolved,
        external: tally.external,
        local_binding: tally.local_binding,
        unresolved: tally.unresolved_total(),
    };
    println!(
        "hcl          resolved {:<8} external {:<8} local-binding {:<8} unresolved {:<8}",
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
    let mut empty_files = 0usize;
    let mut forms: BTreeMap<&str, u64> = BTreeMap::new();
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    let mut blocks: BTreeMap<String, u64> = BTreeMap::new();
    for rel in &owned {
        let source = std::fs::read_to_string(corpus.join(rel))
            .unwrap_or_else(|e| panic!("re-reading {rel}: {e}"));
        if source.is_empty() {
            empty_files += 1;
        }
        let facts = extract(rel, &source);
        re_extracted += facts.refs.len() as u64;
        for r in &facts.refs {
            // The tier-2 contract, checked on real code and not only on a
            // fixture: a call, a type use or an expression-level reference
            // here would put references into a denominator this track cannot
            // resolve.
            assert_eq!(r.kind, RefKind::Import, "{rel}: {}", r.raw_target);
            assert!(!r.locally_bound, "{rel}: {}", r.raw_target);
            // Every reference starts at the module call that wrote it.
            let enclosing = r.enclosing.as_ref().expect("a source has an encloser");
            assert_eq!(enclosing.path[0], "module", "{rel}: {}", r.raw_target);
        }
        // A `source` attribute and its reference are paired by span, so an
        // attribute with no reference would be a silently dropped import.
        assert_eq!(
            facts.header.sources.len(),
            facts.refs.len(),
            "{rel}: module sources and import references disagree",
        );
        for spec in &facts.header.sources {
            *forms.entry(spec.form.name()).or_default() += 1;
        }
        // Every file declares the directory its definitions live in, first,
        // whether or not it declares anything else.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no container",
        );
        for d in &facts.defs {
            *kinds.entry(d.kind.code()).or_default() += 1;
            let block = match d.owner.first() {
                Some(prefix) => prefix.clone(),
                None => "<container>".to_string(),
            };
            *blocks.entry(block).or_default() += 1;
        }
    }
    println!("             forms  {forms:?}");
    println!("             defs   {kinds:?}");
    println!("             blocks {blocks:?}");

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

    assert_eq!(
        empty_files, EMPTY_FILES,
        "the corpus's zero-byte files moved; an empty file is a file, not an error",
    );
    let want: BTreeMap<u8, u64> = DEFS.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(
        kinds, want,
        "the definition census moved; tier 2's own deliverable is mostly \
         definitions and no rate over 24 references can see them",
    );
    let want: BTreeMap<String, u64> = BLOCKS.iter().map(|(b, n)| ((*b).to_string(), *n)).collect();
    assert_eq!(blocks, want, "the block-type census moved");

    // -- the tally, exactly -----------------------------------------------

    assert_eq!(re_extracted, REFERENCES);
    let want: BTreeMap<&str, u64> = FORMS.iter().copied().collect();
    assert_eq!(forms, want, "the module-source form census moved");

    assert_eq!(measured.resolved, 23);
    // The one registry address the corpus writes, and nothing else. `External`
    // sits outside both rate terms, so this count is the one a widening rule
    // would move first — see `track_hcl::resolve` for why "not a local path"
    // is not how the judgement is made.
    assert_eq!(measured.external, 1);
    // Tier 2 emits no expression-level reference, so nothing can name a
    // local. The other bucket outside both rate terms is empty too.
    assert_eq!(measured.local_binding, 0);
    // Zero, and it is not a claim that HCL is easy: every one of the 23
    // resolved references is a relative path into a directory this same
    // snapshot holds, because a Terraform example module and the module it
    // exercises are vendored together by construction. A corpus whose modules
    // pointed outside their own repository would put every one of them in
    // `External`, and one with a stale path would put it here.
    assert_eq!(measured.unresolved, 0);
    assert!(
        reasons.is_empty(),
        "an unresolved reason appeared: {reasons:?}",
    );

    // -- the definitions the store kept, by kind and by name ---------------

    let read = ReadStore::open(&db).expect("the store opens for reading");
    let mut stored: BTreeMap<u8, u64> = BTreeMap::new();
    let mut packages = 0u64;
    let mut externals: Vec<String> = Vec::new();
    let mut multi_file: BTreeSet<String> = BTreeSet::new();
    let mut multi_file_packages = 0u64;
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
            NodeRecord::Package { declarations, .. } => {
                packages += 1;
                let files: BTreeSet<&str> = declarations.iter().map(|d| d.file.as_str()).collect();
                if files.len() > 1 {
                    multi_file_packages += 1;
                }
            }
            NodeRecord::External { package, .. } => externals.push(package),
        }
        Ok(())
    })
    .expect("walking the node table");
    println!("             nodes {stored:?} packages {packages} externals {externals:?}");
    let want: BTreeMap<u8, u64> = STORED.iter().map(|(k, n)| (k.code(), *n)).collect();
    assert_eq!(stored, want, "the stored definition census moved");
    assert_eq!(packages, PACKAGES, "the stored package census moved");
    assert_eq!(
        externals,
        ["terraform-aws-modules/s3-bucket/aws"],
        "the external node set moved",
    );
    assert_eq!(externals.len() as u64, EXTERNALS);

    // Every container is declared by more than one file, and none of them is
    // a collision: being written by every file under it is what a Terraform
    // module is.
    assert_eq!(
        multi_file_packages, PACKAGES,
        "a container declared by one file only",
    );
    assert_eq!(
        multi_file.len() as u64,
        COLLISIONS,
        "two files declared one address: {multi_file:?}",
    );
    assert_eq!(
        report.fqn_collisions, COLLISIONS,
        "the report and the node table disagree about the collision count",
    );

    // -- the named nodes ---------------------------------------------------

    for (fqn, kind, file, line) in PINNED {
        let id = node_id(Domain::Hcl, fqn);
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

    // -- the directory really is the unit ----------------------------------

    let mut edges = 0usize;
    for (target, callers) in EDGES {
        let id = node_id(Domain::Hcl, target);
        let mut got: Vec<String> = read
            .edges_into(&id)
            .unwrap_or_else(|e| panic!("{target}: {e}"))
            .into_iter()
            .map(|(src, kind)| {
                assert_eq!(kind, RefKind::Import.code(), "{target}: not an import edge");
                match read.node(&src).expect("the edge's source") {
                    Some(NodeRecord::Definition { fqn, .. }) => fqn,
                    other => panic!("{target}: an edge starts at {other:?}"),
                }
            })
            .collect();
        got.sort();
        assert_eq!(got, *callers, "the edges into {target} moved");
        edges += got.len();
    }
    assert_eq!(
        edges as u64, measured.resolved,
        "the named edges and the resolved count disagree",
    );
    drop(read);

    // -- the ratchet ------------------------------------------------------

    let text = std::fs::read_to_string(BASELINE).unwrap_or_else(|e| {
        panic!(
            "reading {BASELINE}: {e}; record it with \
             `arthron gate {CORPUS} --language hcl --baseline {BASELINE} --rebase --commit <sha>`"
        )
    });
    let baseline = parse_baseline(&text).unwrap_or_else(|e| panic!("{BASELINE}: {e}"));
    assert_eq!(
        baseline.language,
        Lang::Hcl.name(),
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

/// The module cache is never read, by both of the mechanisms that keep it
/// out.
///
/// `terraform init` unpacks every remote module's **source** into
/// `.terraform/modules/`. Indexing it would mint in-repository containers for
/// code this repository does not own, and a `source` could then resolve
/// against one — a rate that rises for a reason nobody would look for. The
/// walk skips hidden directories, and [`HclLang::skip_dirs`] names this one
/// besides; this asserts the result rather than either mechanism, so that a
/// change to either is caught by the other.
///
/// Needs no corpus, so it runs everywhere.
#[test]
fn the_terraform_module_cache_is_never_read() {
    let tree = tempfile::tempdir().expect("scratch tree");
    let root = tree.path();
    std::fs::write(
        root.join("main.tf"),
        "module \"vpc\" {\n  source = \"./m\"\n}\n",
    )
    .expect("the repository's own file");
    for cached in ["modules/vpc/main.tf", "providers/aws/main.tf"] {
        let path = root.join(".terraform").join(cached);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the cache");
        std::fs::write(path, "resource \"aws_vpc\" \"cached\" {}\n").expect("a cached file");
    }
    let walked = source_files::<HclLang>(root).expect("walking the tree");
    let rel: Vec<String> = walked
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .expect("under the root")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert_eq!(
        rel,
        ["main.tf"],
        "the walk read something under .terraform, which this repository did not write",
    );
}
