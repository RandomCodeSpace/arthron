//! Phase 0: what `composer.json` says about where this repository's names
//! live.
//!
//! PHP separates the namespace a symbol declares from the directory its file
//! sits in, and a manifest mediates between them. PSR-4 maps a namespace
//! *prefix* to one or more directories: with `GuzzleHttp\` mapped to `src/`,
//! the class `GuzzleHttp\Cookie\CookieJar` is expected at
//! `src/Cookie/CookieJar.php`.
//!
//! Two things follow, and both are why this module exists rather than a rule
//! spelled into the resolver:
//!
//! - **Both autoload blocks count.** A package's own sources are under
//!   `autoload`, its tests under `autoload-dev`. A reader of the first block
//!   alone resolves a repository's `src/` tree and drops its `tests/` one —
//!   on the vendored corpus, 68 files read and 63 dropped.
//! - **A prefix this repository declares is a claim on the whole subtree
//!   under it.** That is what lets the resolver tell "outside this
//!   repository" from "inside it, and not there" without guessing.
//!
//! # What is deliberately not read
//!
//! - **Nested `composer.json` files.** Only the manifest at the repository
//!   root is read. A monorepo whose packages each carry one declares prefixes
//!   this build does not see, and their names miss rather than resolve. The
//!   fix is a decision about which root a prefix maps against, not a loop.
//! - **`psr-0`, `classmap` and `files`.** Not in the corpus, so nothing here
//!   has parsed one. `files` is what a project uses to autoload *functions*
//!   and *constants*, which is why the resolver says what it says about a
//!   `use function` miss.
//! - **`require` and `require-dev`.** They name packages, and a package name
//!   does not give its namespace: `guzzlehttp/promises` supplies
//!   `GuzzleHttp\Promise`, which is neither segment studly-cased. Deriving one
//!   from the other would be a guess, and a wrong guess here mints an
//!   `External` that leaves both terms of the resolution rate.

use std::collections::BTreeSet;
use std::path::Path;

/// The PSR-4 map this repository declares, plus the files a scan found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhpProject {
    /// `(namespace prefix, source directories)`, prefix carrying no trailing
    /// `\` and directories no trailing `/`. Sorted by descending prefix
    /// length, so the first match is the longest one — which is the order
    /// PSR-4 resolves in.
    pub psr4: Vec<(String, Vec<String>)>,
    /// Whether a `composer.json` was found and parsed at the repository root.
    /// Recorded for diagnostics; resolution branches on `psr4` being empty,
    /// because a manifest that declares no PSR-4 prefix determines no layout.
    pub manifest: bool,
    /// Every repository-relative `.php` path the walk found.
    pub files: BTreeSet<String>,
}

/// How many namespace segments a PSR-4 prefix carries.
fn depth(prefix: &str) -> usize {
    prefix.split('\\').count()
}

impl PhpProject {
    /// Build a project from a PSR-4 map, ordering the map the way PSR-4
    /// resolves: **longest prefix first**, so the first match
    /// [`PhpProject::claiming_prefix`] finds is the most specific claim. A
    /// repository that maps both `A\` and `A\B\` means the second for a name
    /// under it, and an unsorted map would silently mean the first.
    pub fn new(psr4: Vec<(String, Vec<String>)>, manifest: bool, files: BTreeSet<String>) -> Self {
        let mut psr4 = psr4;
        psr4.sort_by_key(|(prefix, _)| std::cmp::Reverse(depth(prefix)));
        PhpProject {
            psr4,
            manifest,
            files,
        }
    }

    /// Whether this build can say where a name would live. `false` when no
    /// PSR-4 prefix was read at all, which is the one case where a miss is
    /// arthron's own inference rather than a statement about the name.
    pub fn layout_known(&self) -> bool {
        !self.psr4.is_empty()
    }

    /// The longest declared prefix that claims this name, as
    /// `(prefix segment count, directories)`.
    ///
    /// A prefix claims a name only when the name is *longer* than it:
    /// `GuzzleHttp\` maps `GuzzleHttp\Client` and says nothing about a class
    /// literally called `GuzzleHttp`.
    pub fn claiming_prefix(&self, segments: &[String]) -> Option<(usize, &[String])> {
        self.psr4.iter().find_map(|(prefix, dirs)| {
            let want: Vec<&str> = prefix.split('\\').collect();
            (segments.len() > want.len()
                && segments
                    .iter()
                    .zip(&want)
                    .all(|(have, want)| have.as_str() == *want))
            .then_some((want.len(), dirs.as_slice()))
        })
    }

    /// Whether the file PSR-4 would load this class from is in this
    /// repository.
    ///
    /// The difference between "the map points somewhere, and it is empty" and
    /// "the file is here and the name is not" — one is a gap in what this
    /// build knows, the other would be a bug in the extractor or a corpus
    /// that does not run.
    pub fn psr4_file_exists(&self, dirs: &[String], rest: &[String]) -> bool {
        dirs.iter().any(|dir| {
            let tail = format!("{}.php", rest.join("/"));
            let path = if dir.is_empty() {
                tail
            } else {
                format!("{dir}/{tail}")
            };
            self.files.contains(&path)
        })
    }
}

/// Read the repository's `composer.json`, if it has one.
///
/// Never fails: a repository with no manifest, or one whose manifest is not
/// JSON, is still a repository full of PHP whose definitions are extractable.
/// It is the *resolver* that says what an unknown layout costs, once, per
/// reference, with a reason.
pub fn load(root: &Path, files: &[String]) -> PhpProject {
    let files: BTreeSet<String> = files.iter().cloned().collect();
    let Ok(text) = std::fs::read_to_string(root.join("composer.json")) else {
        return PhpProject::new(Vec::new(), false, files);
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
        // Present and unreadable is the same as absent here.
        return PhpProject::new(Vec::new(), false, files);
    };
    let mut psr4: Vec<(String, Vec<String>)> = Vec::new();
    // Top-level blocks only. A `repositories[].package.autoload` entry
    // describes a *dependency's* layout — the vendored corpus carries one —
    // and reading it would map a third-party namespace onto this
    // repository's directories.
    for block in ["autoload", "autoload-dev"] {
        let Some(declared) = manifest.get(block).and_then(|b| b.get("psr-4")) else {
            continue;
        };
        let Some(entries) = declared.as_object() else {
            continue;
        };
        for (prefix, value) in entries {
            let prefix = prefix.trim_end_matches('\\').to_string();
            if prefix.is_empty() {
                // A fallback prefix claims every name there is, which would
                // turn every external import into an in-repository miss.
                continue;
            }
            let dirs = directories(value);
            if dirs.is_empty() {
                continue;
            }
            match psr4.iter_mut().find(|(p, _)| *p == prefix) {
                Some((_, existing)) => existing.extend(dirs),
                None => psr4.push((prefix, dirs)),
            }
        }
    }
    PhpProject::new(psr4, true, files)
}

/// A PSR-4 value is one directory or a list of them.
fn directories(value: &serde_json::Value) -> Vec<String> {
    let one = |v: &serde_json::Value| v.as_str().map(|s| s.trim_end_matches('/').to_string());
    match value {
        serde_json::Value::Array(items) => items.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(psr4: &[(&str, &[&str])]) -> PhpProject {
        PhpProject::new(
            psr4.iter()
                .map(|(p, d)| {
                    (
                        (*p).to_string(),
                        d.iter().map(|s| (*s).to_string()).collect(),
                    )
                })
                .collect(),
            true,
            BTreeSet::new(),
        )
    }

    fn segs(name: &str) -> Vec<String> {
        name.split('\\').map(str::to_string).collect()
    }

    #[test]
    fn a_prefix_claims_the_subtree_under_it_and_not_itself() {
        let p = project(&[("GuzzleHttp", &["src"])]);
        assert_eq!(
            p.claiming_prefix(&segs("GuzzleHttp\\Client")).map(|c| c.0),
            Some(1),
        );
        assert_eq!(p.claiming_prefix(&segs("GuzzleHttp")), None);
        assert_eq!(p.claiming_prefix(&segs("Psr\\Log\\LoggerInterface")), None);
    }

    #[test]
    fn the_longest_prefix_wins() {
        let p = project(&[("GuzzleHttp", &["src"]), ("GuzzleHttp\\Tests", &["tests"])]);
        let (len, dirs) = p
            .claiming_prefix(&segs("GuzzleHttp\\Tests\\Server"))
            .expect("claimed");
        assert_eq!(len, 2);
        assert_eq!(dirs, ["tests"]);
    }

    #[test]
    fn an_empty_map_determines_no_layout() {
        assert!(!PhpProject::default().layout_known());
        assert!(project(&[("A", &["src"])]).layout_known());
    }

    #[test]
    fn psr4_maps_a_name_onto_a_path_under_each_directory() {
        let mut p = project(&[("GuzzleHttp", &["src"])]);
        p.files.insert("src/Cookie/CookieJar.php".to_string());
        assert!(p.psr4_file_exists(&["src".to_string()], &segs("Cookie\\CookieJar")));
        assert!(!p.psr4_file_exists(&["src".to_string()], &segs("Psr7\\Request")));
    }

    #[test]
    fn only_the_top_level_autoload_blocks_are_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("composer.json"),
            r#"{
                "autoload": { "psr-4": { "GuzzleHttp\\": "src/" } },
                "autoload-dev": { "psr-4": { "GuzzleHttp\\Tests\\": "tests/" } },
                "repositories": [
                    { "type": "package", "package": {
                        "autoload": { "psr-4": { "Http\\Client\\Tests\\": "src/" } } } }
                ]
            }"#,
        )
        .expect("write manifest");
        let p = load(dir.path(), &[]);
        assert!(p.manifest);
        assert_eq!(
            p.psr4,
            [
                ("GuzzleHttp\\Tests".to_string(), vec!["tests".to_string()]),
                ("GuzzleHttp".to_string(), vec!["src".to_string()]),
            ],
            "a dependency's own autoload block is not this repository's",
        );
    }

    #[test]
    fn a_missing_or_unreadable_manifest_is_not_a_scan_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let absent = load(dir.path(), &[]);
        assert!(!absent.manifest);
        assert!(!absent.layout_known());

        std::fs::write(dir.path().join("composer.json"), "{ not json").expect("write");
        let broken = load(dir.path(), &[]);
        assert!(!broken.manifest);
        assert!(!broken.layout_known());
    }

    #[test]
    fn a_psr4_value_may_be_a_list_of_directories() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("composer.json"),
            r#"{"autoload":{"psr-4":{"A\\":["src/","lib/"],"":"fallback/"}}}"#,
        )
        .expect("write");
        let p = load(dir.path(), &[]);
        assert_eq!(
            p.psr4,
            [("A".to_string(), vec!["src".to_string(), "lib".to_string()])],
            "a fallback prefix claims every name and is not read",
        );
    }
}
