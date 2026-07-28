//! Stage-1 acceptance for the Java track: the extractor survives a real
//! corpus and the records it emits are well formed.
//!
//! There is deliberately **no rate here**. Java has no resolver yet, so any
//! number this file printed would be a number about nothing. What it can
//! check is the extractor's own contract: every file yields a container, no
//! definition is nameless, and every reference carries a target a resolver
//! could act on.

use std::path::Path;

use arthron::lang::FileFacts;
use arthron::model::{DefKind, RefKind, TargetRoot};
use arthron::pipeline::source_files;
use arthron::track_java::JavaLang;
use arthron::track_java::extract::extract;

mod support;

/// Whether the Java corpus has been cloned in.
///
/// It lives in RandomCodeSpace/arthron-corpus, cloned into ./corpus
/// (gitignored). Skipping is correct when it is absent — failing would make
/// an unfetched corpus look like a broken extractor.
fn corpus_present(corpus: &Path) -> bool {
    if corpus.is_dir() {
        return true;
    }
    support::missing(corpus);
    false
}

/// Every invariant one file's records must satisfy on their own.
fn check_file(rel: &str, facts: &FileFacts<JavaLang>) {
    let container = &facts.defs[0];
    assert_eq!(
        container.kind,
        DefKind::Module,
        "{rel}: the container is not the first definition",
    );
    for def in &facts.defs[1..] {
        assert!(!def.name.is_empty(), "{rel}: a nameless definition");
        assert_ne!(
            def.kind,
            DefKind::Module,
            "{rel}: a second container definition would shadow the first",
        );
    }
    for r in &facts.refs {
        assert!(!r.raw_target.is_empty(), "{rel}: a reference with no text");
        match (&r.target.root, r.target.segments.is_empty()) {
            // `this(…)` and `super(…)` name a constructor on a type the
            // resolver derives from the encloser, so they carry no segments.
            (TargetRoot::This { .. } | TargetRoot::Super { .. }, true) => {
                assert_eq!(r.kind, RefKind::New, "{rel}: {}", r.raw_target);
            }
            (TargetRoot::Name, true) => {
                panic!("{rel}: a name root with no segments: {}", r.raw_target)
            }
            _ => {}
        }
        // M-06: every call and creation site knows its arity, and nothing
        // else claims one.
        let carries_arity = matches!(r.kind, RefKind::Call | RefKind::New);
        assert_eq!(
            r.argc.is_some(),
            carries_arity,
            "{rel}: {:?} `{}` arity",
            r.kind,
            r.raw_target,
        );
    }
}

#[test]
fn the_extractor_survives_the_java_corpus() {
    let corpus = Path::new("corpus/java/commons-lang");
    if !corpus_present(corpus) {
        return;
    }
    let files = source_files::<JavaLang>(corpus).expect("walking the corpus");
    assert!(!files.is_empty(), "the corpus walked to nothing");

    let mut defs = 0u64;
    let mut refs = 0u64;
    let mut kinds = [0u64; 10];
    for path in &files {
        let rel = path
            .strip_prefix(corpus)
            .expect("a walked path is under the corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let facts = extract(&rel, &source);
        check_file(&rel, &facts);
        defs += facts.defs.len() as u64;
        refs += facts.refs.len() as u64;
        for r in &facts.refs {
            kinds[r.kind.code() as usize] += 1;
        }
    }

    // A profile, not a gate: the resolver does not exist, so the only
    // trustworthy statement here is what was seen.
    println!("files      {}", files.len());
    println!("defs       {defs}");
    println!("refs       {refs}");
    for (code, count) in kinds.iter().enumerate() {
        if *count > 0 {
            let kind = RefKind::from_code(code as u8).expect("a counted code");
            println!("  {kind:?} {count}");
        }
    }

    // Floors, deliberately far below what commons-lang contains: they catch a
    // rule file that stopped matching, not a corpus that changed.
    assert!(defs > 1_000, "only {defs} definitions");
    assert!(refs > 10_000, "only {refs} references");
    for kind in [
        RefKind::Call,
        RefKind::Import,
        RefKind::TypeUse,
        RefKind::Inherit,
        RefKind::New,
        RefKind::Annotation,
    ] {
        assert!(
            kinds[kind.code() as usize] > 0,
            "no {kind:?} reference in the whole corpus",
        );
    }
}
