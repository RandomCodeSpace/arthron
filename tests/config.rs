//! `arthron.toml`, end to end: what the file says has to change what the scan
//! reads, or the file is decoration.
//!
//! The parser's own cases — every key, every wrong type, every unknown key —
//! are unit tests in `src/config.rs`. What is here is the part a unit test
//! cannot show: a glob that keeps a file out of a real walk, a `[tracks]`
//! entry that keeps a language out of a real report, and the flag-beats-file
//! precedence as the binary actually applies it.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use arthron::config::Config;
use arthron::model::Lang;
use arthron::pipeline::{scan_repo, scan_repo_with};
use arthron::query::NameIndex;
use arthron::store::{ReadStore, Report};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A module with one hand-written package and one that stands in for
/// generated output, plus a Java file so a `[tracks]` entry has something to
/// switch off.
fn fixture(root: &Path) {
    write(root, "go.mod", "module example.com/app\n\ngo 1.22\n");
    // Each package calls within itself, so both languages have reference rows
    // and therefore a tally: a report keyed off references cannot show a
    // language that never referenced anything.
    write(
        root,
        "app/app.go",
        concat!(
            "package app\n\n",
            "func Live(s string) string { return helper(s) }\n\n",
            "func helper(s string) string { return s }\n",
        ),
    );
    write(
        root,
        "generated/gen.go",
        concat!(
            "package generated\n\n",
            "func Made(s string) string { return inner(s) }\n\n",
            "func inner(s string) string { return s }\n",
        ),
    );
    write(
        root,
        "src/main/java/com/acme/Tool.java",
        concat!(
            "package com.acme;\n\n",
            "public class Tool {\n",
            "    public String run(String s) { return help(s); }\n",
            "    String help(String s) { return s; }\n",
            "}\n",
        ),
    );
}

/// Which of the fixture's definitions the graph holds after a scan under
/// `config`.
///
/// Asked by name through the index's own lookup rather than by reading the
/// node table: a test that reached past the query surface to observe the
/// store would be a second source of truth about what a scan produced.
fn names(root: &Path, db: &Path, config: &Config) -> Vec<String> {
    scan_repo_with(root, db, config).expect("the fixture scans");
    let store = ReadStore::open(db).expect("the store opens read-only");
    let index = NameIndex::build(&store).expect("index builds");
    [
        "example.com/app/app#Live",
        "example.com/app/generated#Made",
        "com.acme#Tool",
    ]
    .into_iter()
    .filter(|fqn| !index.lookup(fqn).matches.is_empty())
    .map(str::to_string)
    .collect()
}

#[test]
fn no_config_file_is_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        Config::load(dir.path()).expect("an absent file is not an error"),
        Config::default(),
    );
}

#[test]
fn a_config_file_that_exists_is_read_from_the_repository_root() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "arthron.toml", "exclude = [\"generated/**\"]\n");
    let config = Config::load(dir.path()).expect("parses");
    assert_eq!(config.exclude, ["generated/**"]);
}

#[test]
fn an_exclude_glob_keeps_a_file_out_of_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    // Without the glob, the generated package is in the graph…
    let plain = names(
        dir.path(),
        &dir.path().join("plain.redb"),
        &Config::default(),
    );
    assert!(
        plain.contains(&"example.com/app/generated#Made".to_string()),
        "{plain:?}",
    );

    // …and with it, the walk never reaches the file, so nothing it declares
    // is a node. The hand-written package is untouched either way.
    let config = Config::parse("exclude = [\"generated/**\"]\n").expect("parses");
    let filtered = names(dir.path(), &dir.path().join("filtered.redb"), &config);
    assert!(
        !filtered.contains(&"example.com/app/generated#Made".to_string()),
        "the excluded file still reached the graph: {filtered:?}",
    );
    assert!(
        filtered.contains(&"example.com/app/app#Live".to_string()),
        "the exclusion took the wrong file: {filtered:?}",
    );
}

#[test]
fn an_include_glob_is_a_whitelist() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let config = Config::parse("include = [\"app/**\"]\n").expect("parses");
    let filtered = names(dir.path(), &dir.path().join("include.redb"), &config);
    assert!(
        filtered.contains(&"example.com/app/app#Live".to_string()),
        "{filtered:?}",
    );
    assert!(
        !filtered.contains(&"example.com/app/generated#Made".to_string()),
        "a file matching no include glob was still read: {filtered:?}",
    );
}

#[test]
fn an_exclude_wins_over_an_include_that_also_matches() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let config =
        Config::parse("include = [\"app/**\", \"generated/**\"]\nexclude = [\"generated/**\"]\n")
            .expect("parses");
    let filtered = names(dir.path(), &dir.path().join("both.redb"), &config);
    assert!(
        filtered.contains(&"example.com/app/app#Live".to_string()),
        "{filtered:?}",
    );
    assert!(
        !filtered.contains(&"example.com/app/generated#Made".to_string()),
        "{filtered:?}",
    );
}

/// Which languages a report holds a tally for.
fn tallied(report: &Report) -> Vec<&'static str> {
    report
        .per_lang
        .keys()
        .filter_map(|code| Lang::from_code(*code).map(Lang::name))
        .collect()
}

#[test]
fn a_tracks_entry_keeps_a_live_language_out_of_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());

    let with_java = scan_repo(dir.path(), &dir.path().join("all.redb")).expect("scans");
    assert!(
        tallied(&with_java).contains(&"java"),
        "the fixture must have Java rows for this test to mean anything: {:?}",
        tallied(&with_java),
    );

    let config = Config::parse("[tracks]\njava = false\n").expect("parses");
    let without =
        scan_repo_with(dir.path(), &dir.path().join("nojava.redb"), &config).expect("scans");
    assert!(
        !tallied(&without).contains(&"java"),
        "a switched-off track still contributed: {:?}",
        tallied(&without),
    );
    assert!(
        tallied(&without).contains(&"go"),
        "switching one track off took another with it: {:?}",
        tallied(&without),
    );
}

#[test]
fn switching_a_track_off_does_not_erase_what_it_already_measured() {
    // A skipped track is not the same as a track handed an empty file set:
    // the second would forget every stored file of its extensions. The rows
    // a previous scan wrote must survive.
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let db = dir.path().join("shared.redb");
    let first = scan_repo(dir.path(), &db).expect("scans");
    assert!(tallied(&first).contains(&"java"));

    let config = Config::parse("[tracks]\njava = false\n").expect("parses");
    let second = scan_repo_with(dir.path(), &db, &config).expect("scans");
    assert!(
        tallied(&second).contains(&"java"),
        "the store forgot rows the config only asked it not to re-measure: {:?}",
        tallied(&second),
    );
}

fn arthron(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arthron"))
        .args(args)
        .output()
        .expect("running the arthron binary")
}

#[test]
fn the_config_names_the_store_and_the_flag_overrules_it() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    write(dir.path(), "arthron.toml", "db = \"from-config.redb\"\n");

    let out = arthron(&["scan", dir.path().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dir.path().join("from-config.redb").exists(),
        "the config's `db` was not used",
    );
    assert!(
        !dir.path().join(".arthron/graph.redb").exists(),
        "the default store was built beside the configured one",
    );

    let flagged = dir.path().join("from-flag.redb");
    let out = arthron(&[
        "scan",
        dir.path().to_str().unwrap(),
        "--db",
        flagged.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(flagged.exists(), "the flag did not win over the file");
}

#[test]
fn an_unknown_key_stops_the_run_and_names_the_key() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    // A silent typo in a config file is how a wrong scan becomes a trusted
    // number, so this is a refusal and not a warning.
    write(dir.path(), "arthron.toml", "exlude = [\"generated/**\"]\n");

    let out = arthron(&["scan", dir.path().to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown key `exlude`"), "{stderr}");
    assert!(stderr.contains("arthron.toml"), "{stderr}");
    assert!(
        !dir.path().join(".arthron/graph.redb").exists(),
        "the scan ran anyway",
    );
}

#[test]
fn a_malformed_config_stops_the_run_rather_than_falling_back_to_defaults() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    write(dir.path(), "arthron.toml", "exclude = [\n");

    let out = arthron(&["scan", dir.path().to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        !dir.path().join(".arthron/graph.redb").exists(),
        "a scan ran on a config nobody could read",
    );
}

#[test]
fn the_gate_reads_the_corpus_config_and_ignores_its_db_key() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    // A gate is only meaningful against a cold store, so the one key that
    // could point it at a warm one is not read.
    write(
        dir.path(),
        "arthron.toml",
        "db = \"gate-should-not-use-this.redb\"\nexclude = [\"generated/**\"]\n",
    );
    let baseline = dir.path().join("go.toml");

    let out = arthron(&[
        "gate",
        dir.path().to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--rebase",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !dir.path().join("gate-should-not-use-this.redb").exists(),
        "the gate measured a store the config named",
    );
    // …and the exclusion was applied, so the gate measures the file set a
    // scan of the same tree measures.
    let recorded = fs::read_to_string(&baseline).expect("the baseline was written");
    assert!(recorded.contains("language = \"go\""), "{recorded}");
}

/// A repository you did not write is still a repository you may scan, and its
/// `arthron.toml` is its text — not yours. The `db` key names where a scan
/// writes, so a value that leaves the scanned tree lets the tree pick a file
/// anywhere this process can write and replace it. The command-line `--db`
/// flag is the other half of the same rule and stays free: you typing a path
/// is you saying it.
#[test]
fn a_config_db_may_not_climb_out_of_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let victim = dir.path().join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("graph.redb"), b"someone else's store\n").unwrap();
    fixture(&repo);
    write(&repo, "arthron.toml", "db = \"../victim/graph.redb\"\n");

    let out = arthron(&["scan", repo.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("inside the repository"),
        "the refusal has to say what the rule is: {stderr}",
    );
    assert!(
        stderr.contains("--db"),
        "the refusal has to say which path is allowed to leave: {stderr}",
    );
    assert_eq!(
        fs::read(victim.join("graph.redb")).unwrap(),
        b"someone else's store\n",
        "the scan wrote over a store outside the tree it was asked to scan",
    );
}

#[test]
fn a_config_db_that_is_absolute_is_refused_however_ordinary_it_looks() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let elsewhere = dir.path().join("elsewhere.redb");
    fixture(&repo);
    write(
        &repo,
        "arthron.toml",
        &format!("db = \"{}\"\n", elsewhere.display()),
    );

    let out = arthron(&["scan", repo.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(!elsewhere.exists(), "the scan wrote outside the repository");
}

/// Lexical containment is not containment: `db = "out/graph.redb"` stays
/// inside the tree as a string and leaves it as a path when `out` is a link.
#[cfg(unix)]
#[test]
fn a_config_db_under_a_symlinked_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fixture(&repo);
    std::os::unix::fs::symlink(&outside, repo.join("out")).unwrap();
    write(&repo, "arthron.toml", "db = \"out/graph.redb\"\n");

    let out = arthron(&["scan", repo.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        !outside.join("graph.redb").exists(),
        "the scan followed a link out of the repository",
    );
}

/// A link with nothing on the other end passes `lstat` and fails
/// `canonicalize`, and the containment walk used to read that failure as
/// "not there yet" and step over it to the parent — which is inside the root,
/// so the whole path was called contained and the store was created *through*
/// the link. One planted link, one file written anywhere this process can
/// write, exit 0, nothing said. The second run was refused, because by then
/// the target existed; the first had already done it.
#[cfg(unix)]
#[test]
fn a_config_db_that_is_a_link_to_nothing_is_refused_before_it_creates_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let victim_parent = dir.path().join("victim_parent");
    fs::create_dir_all(&victim_parent).unwrap();
    fixture(&repo);
    let planted = victim_parent.join("planted.redb");
    // The target does not exist: that is the whole point of the case.
    std::os::unix::fs::symlink(&planted, repo.join("graph.redb")).unwrap();
    write(&repo, "arthron.toml", "db = \"graph.redb\"\n");

    let out = arthron(&["scan", repo.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("inside the repository"),
        "the refusal has to say what the rule is: {stderr}",
    );
    assert!(
        !planted.exists(),
        "the scan created a file outside the repository it was asked to scan",
    );
    // And again, because the hole was self-limiting rather than closed: a
    // second run must be refused for the same reason and not because the
    // first one made the target exist.
    let out = arthron(&["scan", repo.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(!planted.exists(), "the second run created it instead");
}

/// The same hole one level down: `sub/graph.redb` where `sub` exists and
/// `sub/graph.redb` is the dangling link. Lexically inside, and the deepest
/// existing component is inside too.
#[cfg(unix)]
#[test]
fn a_nested_config_db_that_is_a_link_to_nothing_is_refused_too() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fixture(&repo);
    fs::create_dir_all(repo.join("sub")).unwrap();
    let planted = outside.join("planted.redb");
    std::os::unix::fs::symlink(&planted, repo.join("sub/graph.redb")).unwrap();
    write(&repo, "arthron.toml", "db = \"sub/graph.redb\"\n");

    let out = arthron(&["scan", repo.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        !planted.exists(),
        "the scan wrote through the link: {stderr}"
    );
}

/// The flag is the person, the file is the repository. Only one of them is an
/// authority about where this machine's files may be written.
#[test]
fn the_db_flag_may_name_a_store_the_config_may_not() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let outside = dir.path().join("outside.redb");
    fixture(&repo);
    // The same value the config is refused for, typed on the command line.
    write(&repo, "arthron.toml", "db = \"../outside.redb\"\n");

    let out = arthron(&[
        "scan",
        repo.to_str().unwrap(),
        "--db",
        outside.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(outside.exists(), "the flag did not get its way");
}
