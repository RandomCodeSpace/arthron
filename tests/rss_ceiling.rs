//! The guard on the memory non-negotiable: a cold scan must never again hold
//! the whole tree's references.
//!
//! # What this file can prove, and what it cannot
//!
//! It cannot run the measurement that matters. The 512 MiB ceiling is stated
//! against a 5.35M-line Go tree, and no such tree is in
//! `RandomCodeSpace/arthron-corpus` — the tree the ceiling was measured on is
//! a scratch clone that is deliberately not vendored, so **CI never executes
//! that measurement**, neither here nor in `.github/workflows/gate.yml`.
//!
//! Nor is an in-corpus proxy a substitute, and that is measured rather than
//! assumed. On the largest Go corpus the fix this file guards moved peak RSS
//! from 55,840 kB to 56,140 kB — it got 300 kB *worse*, because on a tree
//! that small the peak is fixed cost (the binary, the ast-grep rule set, the
//! store's page cache) and the retained references were never the peak at
//! all. A threshold set on any corpus tree would therefore have passed on the
//! very code this file exists to reject. There is no in-corpus proxy for this
//! ceiling, and pretending otherwise would be a green check for a test that
//! measures nothing.
//!
//! So this file guards the **mechanism** instead, which is the thing a future
//! change would break: the driver does not retain a file's references between
//! the walk and the phase that resolves them. That is exactly what regressed,
//! it is CI-checkable on a synthetic tree in milliseconds, and it fails the
//! moment someone puts `FileFacts` back into the retained record.
//!
//! # The measurement, for a human to run
//!
//! Reference hardware is 2 vCPU; the ceiling is hard, the timing a target:
//!
//! ```text
//! taskset -c 0,1 /usr/bin/time -v ./target/release/arthron scan <tree> --db <scratch>.db
//! ```
//!
//! Against a 17,873-file / 5,353,211-line Go tree yielding 1,678,021 Go
//! references, on the commit that added this file: **286,544 kB peak RSS,
//! 108.9 s wall** — 54.7% of the 524,288 kB ceiling, and 20.4 s per 1M lines
//! against a 60 s target. The commit before it measured 830,612 kB, 158.4% of
//! the ceiling. `docs/decisions.md` carries the full table.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Mutex;

use arthron::config::FileFilter;
use arthron::extract_go::GoExtractor;
use arthron::lang::{Extractor, FileFacts, Language};
use arthron::pipeline::scan;
use arthron::resolve_go::{GoLang, GoResolver};
use arthron::track_java::extract::JavaExtractor;
use arthron::track_java::{JavaLang, JavaResolver};

/// A real extractor, wrapped in a tally of how often each file reached it.
///
/// The count is the whole assertion. A driver that holds a file's references
/// extracts it once; a driver that re-reads them extracts it again per phase
/// that wants them.
struct Counting<L: Language, E: Extractor<L>> {
    inner: E,
    calls: Mutex<BTreeMap<String, usize>>,
    lang: PhantomData<fn() -> L>,
}

impl<L: Language, E: Extractor<L>> Counting<L, E> {
    fn new(inner: E) -> Self {
        Counting {
            inner,
            calls: Mutex::new(BTreeMap::new()),
            lang: PhantomData,
        }
    }

    /// The tally, as `path -> times extracted`.
    fn tally(&self) -> BTreeMap<String, usize> {
        self.calls
            .lock()
            .expect("the tally is not poisoned")
            .clone()
    }
}

impl<L: Language, E: Extractor<L>> Extractor<L> for Counting<L, E> {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<L> {
        *self
            .calls
            .lock()
            .expect("the tally is not poisoned")
            .entry(rel_path.to_string())
            .or_default() += 1;
        self.inner.extract(rel_path, source)
    }
}

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("a parent directory")).expect("mkdir");
    std::fs::write(path, content).expect("write");
}

/// A language with no link kinds runs two phases over a changed file, and
/// only the second reads its references.
///
/// So every file is extracted exactly twice on a cold scan: once by the walk,
/// which keeps its declarations and throws its references away, and once by
/// phase 2, which is where every reference in the tree is resolved and then
/// dropped before the next file is read.
///
/// **A count of 1 is the regression this file exists for.** It means the
/// walk's references were kept alive until phase 2 consumed them — every
/// file's, at once, which on a large tree was 89.8% of peak RSS and 1.59x
/// over the 512 MiB ceiling.
#[test]
fn go_extracts_each_changed_file_twice_because_phase_2_re_reads_it() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let root = dir.path();
    write(root, "go.mod", "module example.com/m\n\ngo 1.21\n");
    write(
        root,
        "a/a.go",
        "package a\n\nfunc Alpha() int { return 1 }\n",
    );
    write(
        root,
        "b/b.go",
        "package b\n\nimport \"example.com/m/a\"\n\nfunc Beta() int { return a.Alpha() }\n",
    );
    write(
        root,
        "c/c.go",
        "package c\n\nimport \"example.com/m/b\"\n\nfunc Gamma() int { return b.Beta() }\n",
    );

    let ex = Counting::new(GoExtractor);
    scan::<GoLang>(
        root,
        &root.join("scan.db"),
        &ex,
        &GoResolver,
        &FileFilter::none(),
    )
    .expect("the scan succeeds");

    let tally = ex.tally();
    assert_eq!(
        tally.len(),
        3,
        "every owned Go file should have been extracted: {tally:?}",
    );
    for (path, times) in &tally {
        assert_eq!(
            *times, 2,
            "{path} was extracted {times} times, not twice. One means the driver held \
             this file's references from the walk until phase 2 resolved them, which \
             is the regression that put a cold scan 1.59x over the 512 MiB ceiling — \
             see this file's module comment. More than two means a phase was added \
             that re-reads and nobody re-measured the wall clock.",
        );
    }
}

/// A language whose resolver declares link kinds runs a supertype phase
/// between the two, and that phase reads references too — so its files are
/// extracted three times, not twice.
///
/// Pinned separately because it is the *other* half of the same contract: the
/// supertype phase must not be the one place that keeps the tree alive, and a
/// count of 2 here would mean it had been given back the walk's references.
#[test]
fn java_extracts_each_changed_file_three_times_because_the_supertype_phase_re_reads_too() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let root = dir.path();
    write(
        root,
        "src/main/java/p/Base.java",
        "package p;\n\npublic class Base { public int f() { return 1; } }\n",
    );
    write(
        root,
        "src/main/java/p/Derived.java",
        "package p;\n\npublic class Derived extends Base { public int g() { return f(); } }\n",
    );

    let ex = Counting::new(JavaExtractor);
    scan::<JavaLang>(
        root,
        &root.join("scan.db"),
        &ex,
        &JavaResolver,
        &FileFilter::none(),
    )
    .expect("the scan succeeds");

    let tally = ex.tally();
    assert_eq!(
        tally.len(),
        2,
        "every owned Java file should have been extracted: {tally:?}",
    );
    for (path, times) in &tally {
        assert_eq!(
            *times, 3,
            "{path} was extracted {times} times, not three. Java declares link kinds, \
             so the walk, the supertype phase and phase 2 each read this file — and \
             none of them may hold another's references. See this file's module \
             comment.",
        );
    }
}
