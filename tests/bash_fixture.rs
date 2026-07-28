//! The Bash track end to end, over a tree written to disk.
//!
//! # Why this file exists beside the corpus test
//!
//! `tests/bash_corpus.rs` measures the gated corpus, and that corpus resolves
//! **nothing**: bats-core was vendored precisely because not one of its
//! `source` targets is a literal path, so its honest rate is 0.0% over a
//! denominator of 6. That leaves the only branch of
//! `BashResolver::literal` that can ever return `Resolved` exercised by zero
//! gated rows — a regression in the path normalizer, in the root anchoring,
//! or in the agreement between the identity the definition phase files and
//! the one the reference phase probes would move no rate, no bucket and no
//! baseline, and the ratchet would pass.
//!
//! `tests/bash_resolve.rs` covers the same branch, but against a symbol table
//! a test hands the resolver directly. It cannot catch the two phases
//! disagreeing, because only one of them runs.
//!
//! So this file drives the **whole track** — the walk, both phases, the store
//! — over a tree written here, and it is the only place a Bash `Resolved` row
//! is produced through the store at all. It needs no corpus, so it runs
//! everywhere, and it is where the resolving half of the import model is
//! pinned.
//!
//! # What the tree is built to catch
//!
//! - **The anchor.** Bash resolves `source` against the **working
//!   directory**, never against the sourcing file's own directory.
//!   `sub/rel.bash` writes `source c.bash` while `sub/c.bash` sits right
//!   beside it: the shell would not read that file and neither may this, so
//!   the row must miss. That single assertion is the whole rule, and nothing
//!   in the gated corpus can make it.
//! - **The normalizer.** `./`, an interior `..`, and a `..` that climbs past
//!   the root are three different answers.
//! - **The two spellings and the quoting.** `.` is `source`, and a specifier
//!   is a literal whether it is bare, single-quoted, double-quoted without an
//!   expansion, or concatenated out of pieces. All six spellings below name
//!   two files between them.
//! - **The extension fence.** `sub/case.bats` is on disk and is *not* owned,
//!   so a `source` naming it finds nothing. A track that widened its
//!   extension list would turn that miss into a hit here.
//! - **The file-qualified FQN.** `b.sh` and `sub/c.bash` each write
//!   `usage()`. They are two nodes, because a bash function's identity
//!   carries the file that declares it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use arthron::model::{Lang, NodeId, RefKind, reason_name};
use arthron::store::{NodeRecord, ReadStore, StoredOutcome};
use arthron::track_bash::resolve::scan_bash;

/// Write a file, creating its parent directories.
fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
}

/// A tree exercising every shape `BashResolver::literal` has a rule for,
/// laid out so that each rule's *negative* is one file away from its
/// positive.
fn tree(root: &Path) {
    write(
        root,
        "entry.sh",
        // 1-4: one file named four ways — bare, `./`-prefixed, through an
        //      interior `..`, and double-quoted with nothing to expand.
        // 5-6: the POSIX spelling of the builtin, and a literal written in
        //      two pieces.
        // 7:   a bare name that *is* at the root, so the probe hits.
        // 8:   a bare name that is not, so it belongs to `$PATH`.
        // 9:   a path inside the tree that names no file.
        // 10:  a file that exists on disk under an extension nobody owns.
        // 11:  absolute, so outside this repository by construction.
        // 12:  climbs above the root, so outside it too.
        // 13-15: three specifiers the shell would expand.
        "source lib/a.bash\n\
         source ./lib/a.bash\n\
         source sub/../lib/a.bash\n\
         source \"lib/a.bash\"\n\
         . 'lib/b.sh'\n\
         source lib/'b'.sh\n\
         source b.sh\n\
         source helper\n\
         source lib/missing.bash\n\
         source sub/case.bats\n\
         source /etc/profile\n\
         source ../outside.sh\n\
         source \"$PKG/y.bash\"\n\
         source lib/*.bash\n\
         source ~/rc.bash\n",
    );
    write(
        root,
        // The anchor, stated as a file. `sub/c.bash` is this file's own
        // sibling; `source c.bash` must not find it, and `source sub/d.bash`
        // — written from the working directory, which is how the shell reads
        // it — must.
        "sub/rel.bash",
        "source c.bash\nsource sub/d.bash\n",
    );
    // A function written inside another: the owner chain, end to end.
    write(root, "lib/a.bash", "a_fn() {\n  inner_fn() { :; }\n}\n");
    // The `function` keyword spelling.
    write(root, "lib/b.sh", "function b_fn { :; }\n");
    // One name, two files, two nodes.
    write(root, "b.sh", "usage() { :; }\n");
    write(root, "sub/c.bash", "usage() { :; }\n");
    write(root, "sub/d.bash", "d_fn() { :; }\n");
    // Not an owned extension: never walked, never a node, never a target.
    write(root, "sub/case.bats", "@test \"x\" { :; }\n");
}

/// Every stored row, keyed by `(file, raw target)`, showing what it resolved
/// to or why it did not.
fn rows(db: &Path) -> BTreeMap<(String, String), String> {
    let store = ReadStore::open(db).expect("the store opens");
    let mut nodes: BTreeMap<NodeId, String> = BTreeMap::new();
    store
        .for_each_node(|id, record| {
            let fqn = match record {
                NodeRecord::Package { import_path, .. } => import_path,
                NodeRecord::Definition { fqn, .. } => fqn,
                NodeRecord::External { package, .. } => format!("external:{package}"),
            };
            nodes.insert(id, fqn);
            Ok(())
        })
        .expect("nodes");
    let mut out = BTreeMap::new();
    store
        .for_each_row(|key, record| {
            // The tier-2 contract at the store level: every stored row is an
            // import reference and none is a local binding.
            assert_eq!(key.kind, RefKind::Import.code(), "{key:?}");
            assert!(!key.locally_bound, "{key:?}");
            let shown = match record.outcome {
                StoredOutcome::Resolved(id) => nodes
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| "resolved:<unknown node>".to_string()),
                StoredOutcome::External(pkg) => format!("external:{pkg}"),
                StoredOutcome::Unresolved(code) => format!("unresolved:{}", reason_name(code)),
            };
            out.insert((key.file, key.raw_target), shown);
            Ok(())
        })
        .expect("rows");
    out
}

#[test]
fn the_bash_track_resolves_a_sourced_tree_end_to_end() {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let root = scratch.path();
    tree(root);
    let db = root.join("graph.redb");
    let report = scan_bash(root, &db).expect("the tree scans");
    let tally = report
        .per_lang
        .get(&Lang::Bash.code())
        .cloned()
        .unwrap_or_default();

    let rows = rows(&db);
    for ((file, raw), shown) in &rows {
        println!("{file:16} {raw:26} {shown}");
    }

    let got = |file: &str, raw: &str| {
        rows.get(&(file.to_string(), raw.to_string()))
            .unwrap_or_else(|| panic!("no row for {file} {raw}"))
            .as_str()
    };
    let entry = "entry.sh";

    // -- a literal path resolves, and the normalizer agrees with the walk --

    // The branch the gated corpus cannot reach: a literal specifier, probed
    // at the root, hitting the node the definition phase filed for that
    // file. `$lib/a.bash` is the identity both phases have to spell the same
    // way, and this is the only test in the tree that makes them.
    assert_eq!(got(entry, "source lib/a.bash"), "$lib/a.bash");
    assert_eq!(got(entry, "source ./lib/a.bash"), "$lib/a.bash");
    assert_eq!(got(entry, "source sub/../lib/a.bash"), "$lib/a.bash");
    // Quoting changes what the shell *reads*, never what it names.
    assert_eq!(got(entry, "source \"lib/a.bash\""), "$lib/a.bash");
    // `.` is `source` spelled the POSIX way, and a literal may be written in
    // pieces.
    assert_eq!(got(entry, ". 'lib/b.sh'"), "$lib/b.sh");
    assert_eq!(got(entry, "source lib/'b'.sh"), "$lib/b.sh");

    // A bare name is probed at the root before it is given up on, and a hit
    // there resolves like any other literal. Only the *miss* belongs to
    // `$PATH`.
    assert_eq!(got(entry, "source b.sh"), "$b.sh");
    assert_eq!(got(entry, "source helper"), "unresolved:UnknownPackage");

    // -- the two shapes of a miss, which are two reasons ------------------

    // Inside the tree, so the lookup was complete and the name was not
    // there...
    assert_eq!(
        got(entry, "source lib/missing.bash"),
        "unresolved:ModuleNotFound"
    );
    // ...including a file that is on disk under an extension this track does
    // not own. It was never walked, so nothing declares it.
    assert_eq!(
        got(entry, "source sub/case.bats"),
        "unresolved:ModuleNotFound"
    );
    // Outside the tree, two ways. Never `External`: this track mints none,
    // and both of these count *against* the rate.
    assert_eq!(
        got(entry, "source /etc/profile"),
        "unresolved:UnknownPackage"
    );
    assert_eq!(
        got(entry, "source ../outside.sh"),
        "unresolved:UnknownPackage"
    );

    // -- a specifier the shell would expand is never guessed at -----------

    // A parameter expansion, a glob, and a tilde. Each names a real file in
    // this tree once the shell has run; none of them names one now.
    assert_eq!(
        got(entry, "source \"$PKG/y.bash\""),
        "unresolved:DynamicModuleSpecifier"
    );
    assert_eq!(
        got(entry, "source lib/*.bash"),
        "unresolved:DynamicModuleSpecifier"
    );
    assert_eq!(
        got(entry, "source ~/rc.bash"),
        "unresolved:DynamicModuleSpecifier"
    );

    // -- the anchor is the working directory, not the sourcing file -------

    // `sub/c.bash` is `sub/rel.bash`'s own sibling and it *is* indexed —
    // `$sub/c.bash#usage` is asserted below — so the only thing stopping
    // this row resolving is the rule itself. A resolver that probed the
    // sourcing file's directory would turn this line green and would resolve
    // references the shell never would.
    assert_eq!(
        got("sub/rel.bash", "source c.bash"),
        "unresolved:UnknownPackage"
    );
    // The same file named the way the shell would actually read it.
    assert_eq!(got("sub/rel.bash", "source sub/d.bash"), "$sub/d.bash");

    // -- the tally, exactly ------------------------------------------------

    // Seventeen references and no row lost between them: the resolver never
    // drops, so every reference the extractor emitted is in exactly one
    // bucket here.
    assert_eq!(rows.len(), 17, "one row per site");
    assert_eq!(tally.resolved, 8);
    assert_eq!(tally.external, 0, "this track mints no external node");
    assert_eq!(tally.local_binding, 0, "tier 2 emits no local binding");
    let reasons: BTreeMap<&str, u64> = tally
        .unresolved
        .iter()
        .map(|(code, n)| (reason_name(*code), *n))
        .collect();
    assert_eq!(
        reasons,
        [
            ("DynamicModuleSpecifier", 3),
            ("ModuleNotFound", 2),
            ("UnknownPackage", 4),
        ]
        .into_iter()
        .collect::<BTreeMap<&str, u64>>(),
    );
    assert_eq!(
        tally.resolved + tally.unresolved_total(),
        rows.len() as u64,
        "every reference is in exactly one bucket",
    );

    // -- the identities the definition phase filed -------------------------

    let store = ReadStore::open(&db).expect("the store opens");
    let mut functions: BTreeSet<String> = BTreeSet::new();
    let mut scripts: BTreeSet<String> = BTreeSet::new();
    store
        .for_each_node(|_, record| {
            match &record {
                NodeRecord::Definition { fqn, .. } => {
                    functions.insert(fqn.clone());
                }
                NodeRecord::Package { import_path, .. } => {
                    scripts.insert(import_path.clone());
                }
                NodeRecord::External { .. } => panic!("this track mints no external node"),
            }
            Ok(())
        })
        .expect("nodes");
    drop(store);

    // One script node per owned file, and none for the `.bats` file beside
    // them — which is what makes `source sub/case.bats` a miss above.
    assert_eq!(
        scripts,
        [
            "$b.sh",
            "$entry.sh",
            "$lib/a.bash",
            "$lib/b.sh",
            "$sub/c.bash",
            "$sub/d.bash",
            "$sub/rel.bash",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<String>>(),
    );
    // A function is qualified by the file that writes it and by the chain of
    // functions enclosing it, so `usage` twice is two nodes and `inner_fn`
    // carries `a_fn`.
    assert_eq!(
        functions,
        [
            "$b.sh#usage",
            "$lib/a.bash#a_fn",
            "$lib/a.bash#a_fn.inner_fn",
            "$lib/b.sh#b_fn",
            "$sub/c.bash#usage",
            "$sub/d.bash#d_fn",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<String>>(),
    );

    // Nothing in this tree declares one FQN from two files, so the collision
    // counter is silent — the two `usage` declarations are two identities,
    // not one collision.
    assert_eq!(report.fqn_collisions, 0);
    assert!(report.file_errors.is_empty(), "{:?}", report.file_errors);
}
