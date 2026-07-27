//! Extractor acceptance for Python: every corpus file yields records, and
//! the invariants that decide the resolution rate hold on real code.
//!
//! Skipped when the corpus is absent — it lives in
//! RandomCodeSpace/arthron-corpus, cloned into `./corpus` (gitignored), and
//! failing on an unfetched corpus would make a missing clone look like a
//! broken extractor.

use std::collections::BTreeMap;
use std::path::Path;

use arthron::model::{DefKind, RefKind, TargetRoot};
use arthron::pipeline::source_files;
use arthron::track_python::extract::extract;
use arthron::track_python::lang::PyLang;

#[test]
fn the_extractor_reads_the_python_corpus_without_losing_its_invariants() {
    let corpus = Path::new("corpus/python");
    if !corpus.is_dir() {
        println!("SKIP: no corpus at {} — see README", corpus.display());
        return;
    }
    let files = source_files::<PyLang>(corpus).expect("walking the corpus");
    assert!(!files.is_empty(), "the corpus has no .py files");

    let mut files_read = 0u64;
    let mut defs = 0u64;
    let mut refs = 0u64;
    let mut by_ref_kind: BTreeMap<u8, u64> = BTreeMap::new();
    let mut locally_bound = 0u64;

    for path in &files {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(source) = std::fs::read_to_string(path) else {
            continue; // a file that is not UTF-8 is not Python this build reads
        };
        let facts = extract(&rel, &source);
        files_read += 1;

        // Every file declares the container its definitions live in, whether
        // or not anything else parsed.
        assert_eq!(
            facts.defs.first().map(|d| d.kind),
            Some(DefKind::Module),
            "{rel} declares no module",
        );
        defs += facts.defs.len() as u64;
        refs += facts.refs.len() as u64;
        for r in &facts.refs {
            *by_ref_kind.entry(r.kind.code()).or_default() += 1;
            if r.locally_bound {
                locally_bound += 1;
                // Only a name root consults the binding tables: `self.m()`
                // names an attribute of a class, which is a node.
                assert_eq!(r.target.root, TargetRoot::Name, "{rel}: {}", r.raw_target);
            }
        }
        // An import clause and its reference are paired by span, so a clause
        // with no reference would be a silently dropped import.
        let import_refs = facts
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Import)
            .count();
        assert_eq!(
            import_refs,
            facts.header.imports.len(),
            "{rel}: import clauses and import references disagree",
        );
    }

    println!("files {files_read}  defs {defs}  refs {refs}  locally_bound {locally_bound}");
    for (code, n) in &by_ref_kind {
        println!("  ref kind {code}: {n}");
    }
    assert!(defs > 0 && refs > 0);
}
