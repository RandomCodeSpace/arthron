//! `arthron.toml`: a repository's own answers about what a scan reads.
//!
//! Every key is optional and the file itself is optional, so a repository that
//! has none behaves exactly as it did before this module existed — which is
//! the property the corpus gates depend on.
//!
//! # Unknown keys are a hard error
//!
//! A typo in a config file is silent by default in almost every tool, and the
//! result is a scan that reads a different tree than the author believes it
//! reads while reporting a number they then trust. So an unrecognised key is
//! refused by name rather than ignored, at both levels: a top-level key, and a
//! track name under `[tracks]`.
//!
//! # `[tracks]` switches off, never on
//!
//! A repository may say `java = false` to keep a live track out of its scans.
//! It may not say `rust = true`: a track this build does not implement cannot
//! be conjured by config, and accepting the key would promise a language the
//! binary cannot resolve. `true` for a track that is already live is accepted
//! and means what it says.
//!
//! # What the globs feed
//!
//! [`FileFilter`] is compiled straight into the `ignore` walk the scan already
//! uses, so an excluded directory is pruned rather than walked and discarded.
//! `include` is a whitelist: with any include glob present, a file matching
//! none of them is not read. `exclude` wins over `include` when both match, by
//! the same last-match-wins rule a `.gitignore` uses.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use ignore::overrides::{Override, OverrideBuilder};

use crate::registry::REGISTRY;

/// The file a repository states its scan settings in, at its root.
pub const CONFIG_FILE: &str = "arthron.toml";

/// Why a `db` value that leaves the scanned tree is refused.
///
/// One constant so the refusal, the `--help` text and the tests cannot drift
/// apart about what the rule is.
pub const DB_OUTSIDE_ROOT: &str = "`db` must name a file inside the repository being scanned";

/// Every key [`CONFIG_FILE`] accepts at the top level, in the order the
/// "known keys" half of an error message lists them.
const KEYS: &[&str] = &["db", "exclude", "include", "tracks"];

/// A repository's scan settings, as [`CONFIG_FILE`] states them.
///
/// [`Config::default`] is the no-file case and the no-key case alike: no
/// globs, no track switched off, no database path. It must stay behaviourally
/// identical to the code path that existed before configuration did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Glob patterns a file must match to be read at all. Empty means "no
    /// whitelist", which is not the same as "match nothing".
    pub include: Vec<String>,
    /// Glob patterns that keep a file out of the scan. Applied after
    /// [`Config::include`] and winning over it.
    pub exclude: Vec<String>,
    /// Where the graph lives, relative to the repository root. A `--db` flag
    /// wins over this.
    pub db: Option<PathBuf>,
    /// Track name → whether it may run. Only names
    /// [`crate::registry::REGISTRY`] holds, and only `false` for a track that
    /// is not live.
    pub tracks: BTreeMap<String, bool>,
}

impl Config {
    /// Read `<root>/arthron.toml`, or the defaults when there is no such file.
    ///
    /// An absent file is not an error — most repositories will never have
    /// one. Any other I/O failure is: a file that exists and cannot be read
    /// must not be silently replaced by defaults, because the scan that
    /// followed would measure the wrong tree.
    pub fn load(root: &Path) -> Result<Config, String> {
        let path = root.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("reading {}: {e}", path.display())),
        }
    }

    /// Parse the contents of an [`CONFIG_FILE`].
    ///
    /// Errors carry the offending key by name; see the module header for why
    /// an unknown key is refused rather than ignored.
    pub fn parse(text: &str) -> Result<Config, String> {
        let table: toml::Table = text.parse().map_err(|e| format!("{e}"))?;
        let mut config = Config::default();
        for (key, value) in &table {
            match key.as_str() {
                "include" => config.include = globs(key, value)?,
                "exclude" => config.exclude = globs(key, value)?,
                "db" => {
                    let path = value.as_str().ok_or_else(|| {
                        format!("`db` must be a string, not {}", value.type_str())
                    })?;
                    if path.is_empty() {
                        return Err("`db` is empty; remove the key instead".to_string());
                    }
                    config.db = Some(PathBuf::from(path));
                }
                "tracks" => {
                    let table = value.as_table().ok_or_else(|| {
                        format!("`tracks` must be a table, not {}", value.type_str())
                    })?;
                    config.tracks = tracks(table, &live_tracks())?;
                }
                other => {
                    return Err(format!(
                        "unknown key `{other}`; known keys: {}",
                        KEYS.join(", ")
                    ));
                }
            }
        }
        Ok(config)
    }

    /// Whether the named track may contribute to a scan.
    ///
    /// Unmentioned tracks run; the map only ever takes one away.
    pub fn track_enabled(&self, name: &str) -> bool {
        self.tracks.get(name).copied().unwrap_or(true)
    }

    /// Compile [`Config::include`] and [`Config::exclude`] against a
    /// repository root.
    pub fn filter(&self, root: &Path) -> Result<FileFilter, String> {
        FileFilter::new(root, &self.include, &self.exclude)
    }

    /// The configured database path, resolved against the repository root —
    /// and refused when it is not under it.
    ///
    /// Relative to the repository rather than to the working directory, so the
    /// same file means the same store from wherever `arthron` is run. Inside
    /// it, always: `db` says where a scan *writes*, and a scan reads
    /// repositories this machine did not write. A value free to leave the root
    /// would let a scanned tree pick any file this process can open and
    /// replace it, which is the whole of the attack — so an absolute path, a
    /// `..` that climbs past the root, and a component that is a symbolic link
    /// out of the tree are all refused by name.
    ///
    /// The command-line `--db` flag is under no such rule and never passes
    /// through here. The person typing a path is the authority about this
    /// machine's files; a file sitting inside a scanned tree is not.
    pub fn db_path(&self, root: &Path) -> Result<Option<PathBuf>, String> {
        self.db.as_deref().map(|db| contained(root, db)).transpose()
    }
}

/// `db` resolved against `root`, or why it does not stay inside it.
///
/// Two passes, because either alone leaves a hole. The lexical pass drops
/// `.`, applies `..` and refuses anything absolute or climbing past the root;
/// its *result* is what gets used, so `link/../graph.redb` names
/// `<root>/graph.redb` whatever `link` points at. The filesystem pass then
/// canonicalises the deepest part of that result which exists and checks it is
/// still under the canonical root, which is what catches a directory inside
/// the repository that is a symbolic link out of it. The deepest *existing*
/// part is what there is to ask about: the store itself usually does not exist
/// until the scan creates it — and "does not exist" means `symlink_metadata`
/// finds nothing, not merely that `canonicalize` failed, because a link with
/// nothing on the other end fails the second while passing the first.
fn contained(root: &Path, db: &Path) -> Result<PathBuf, String> {
    let refuse = |why: &str| {
        Err(format!(
            "{DB_OUTSIDE_ROOT}; `{}` {why}. A scan reads repositories this \
             machine did not write, so the repository does not get to choose \
             which of this machine's files a scan replaces — pass `--db` on \
             the command line to put the store outside the scanned tree.",
            db.display(),
        ))
    };
    let mut kept: Vec<OsString> = Vec::new();
    for component in db.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => kept.push(part.to_os_string()),
            Component::ParentDir => {
                if kept.pop().is_none() {
                    return refuse("climbs out of it");
                }
            }
            Component::RootDir | Component::Prefix(_) => return refuse("is an absolute path"),
        }
    }
    if kept.is_empty() {
        return refuse("names the repository root itself rather than a file in it");
    }
    let path = root.join(kept.iter().collect::<PathBuf>());
    // A root that cannot be resolved is a root the scan is about to fail on
    // anyway, and guessing at containment against a path that is not there
    // would refuse valid configurations. Lexical containment stands on its own.
    let Ok(real_root) = root.canonicalize() else {
        return Ok(path);
    };
    let mut probe = path.clone();
    loop {
        match probe.canonicalize() {
            Ok(real) if real.starts_with(&real_root) => return Ok(path),
            Ok(_) => return refuse("resolves outside it"),
            // `canonicalize` fails for two different reasons, and only one of
            // them means "not there yet". A component `symlink_metadata`
            // answers for is a component that *exists* — a symbolic link
            // whose target does not is the ordinary case — and walking past
            // one to its parent would find the parent inside the root and
            // call the whole path contained. `Store::open` then creates the
            // file *through* the link, at whatever path the scanned
            // repository put on the other end of it, which is the one thing
            // this function exists to prevent. Once is enough: the target
            // exists after the first scan, so only the second is refused.
            //
            // Refusing is the safe direction here whatever the cause. A
            // component that will not resolve cannot be shown to stay inside
            // the root, and the reader's fix is the same either way — name
            // the store on the command line, or point the link at a file.
            Err(_) if probe.symlink_metadata().is_ok() => {
                return refuse(
                    "has a component that exists but does not resolve, usually a \
                     symbolic link with nothing on the other end, so nothing shows \
                     where it would be written",
                );
            }
            // Not there yet: ask the same question of its parent, down to the
            // root, which does exist.
            Err(_) => match probe.parent() {
                Some(parent) if parent != probe => probe = parent.to_path_buf(),
                _ => return Ok(path),
            },
        }
    }
}

/// Every live track's name, for validating a `[tracks]` entry.
fn live_tracks() -> Vec<(&'static str, bool)> {
    REGISTRY.iter().map(|t| (t.name, t.is_enabled())).collect()
}

/// One `[tracks]` table, checked against what this build actually has.
///
/// Split out from [`Config::parse`] so the rule can be tested against a
/// synthetic registry: every track in this build is live today, so the
/// "cannot be switched on" case has no real subject to exercise it.
fn tracks(table: &toml::Table, known: &[(&str, bool)]) -> Result<BTreeMap<String, bool>, String> {
    let mut out = BTreeMap::new();
    for (name, value) in table {
        let Some((_, live)) = known.iter().find(|(known, _)| known == name) else {
            let names: Vec<&str> = known.iter().map(|(n, _)| *n).collect();
            return Err(format!(
                "unknown key `tracks.{name}`; known tracks: {}",
                names.join(", ")
            ));
        };
        let enable = value.as_bool().ok_or_else(|| {
            format!(
                "`tracks.{name}` must be a boolean, not {}",
                value.type_str()
            )
        })?;
        if enable && !live {
            return Err(format!(
                "`tracks.{name} = true` cannot switch on a track this build does not \
                 implement; the table may only switch a live track off",
            ));
        }
        out.insert(name.clone(), enable);
    }
    Ok(out)
}

/// One glob list, rejecting anything that is not an array of strings.
fn globs(key: &str, value: &toml::Value) -> Result<Vec<String>, String> {
    let array = value.as_array().ok_or_else(|| {
        format!(
            "`{key}` must be an array of strings, not {}",
            value.type_str()
        )
    })?;
    let mut out = Vec::with_capacity(array.len());
    for item in array {
        let glob = item
            .as_str()
            .ok_or_else(|| format!("`{key}` must hold strings; found {}", item.type_str()))?;
        if glob.is_empty() {
            return Err(format!("`{key}` holds an empty glob"));
        }
        out.push(glob.to_string());
    }
    Ok(out)
}

/// Compiled include/exclude globs, as the walk consumes them.
///
/// A newtype over the `ignore` crate's override set rather than a matcher of
/// this module's own: the walk is already an `ignore` walk, and giving it the
/// patterns directly is what makes an excluded directory pruned instead of
/// walked and thrown away.
#[derive(Debug, Clone)]
pub struct FileFilter(Override);

impl FileFilter {
    /// The filter that changes nothing: every file the walk finds is read.
    ///
    /// Identical to the walk's own default, which is what lets every existing
    /// entry point keep its two-argument signature and its exact behaviour.
    pub fn none() -> FileFilter {
        FileFilter(Override::empty())
    }

    /// Compile a filter from glob lists stated relative to `root`.
    ///
    /// Include globs are whitelist patterns and exclude globs are their
    /// inverse, which is why the exclusions are added last: `ignore` resolves
    /// a path by the last pattern that matches it.
    pub fn new(root: &Path, include: &[String], exclude: &[String]) -> Result<FileFilter, String> {
        if include.is_empty() && exclude.is_empty() {
            return Ok(FileFilter::none());
        }
        let mut builder = OverrideBuilder::new(root);
        for glob in include {
            builder
                .add(glob)
                .map_err(|e| format!("include glob `{glob}`: {e}"))?;
        }
        for glob in exclude {
            builder
                .add(&format!("!{glob}"))
                .map_err(|e| format!("exclude glob `{glob}`: {e}"))?;
        }
        let compiled = builder
            .build()
            .map_err(|e| format!("compiling the include/exclude globs: {e}"))?;
        Ok(FileFilter(compiled))
    }

    /// Whether this filter would keep every file the walk finds.
    pub fn is_none(&self) -> bool {
        self.0.is_empty()
    }

    /// The override set, for the walk builder.
    pub(crate) fn overrides(&self) -> Override {
        self.0.clone()
    }
}

impl Default for FileFilter {
    fn default() -> FileFilter {
        FileFilter::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(Config::parse("").expect("empty parses"), Config::default());
    }

    #[test]
    fn every_key_is_optional_and_read() {
        let config = Config::parse(
            "include = [\"src/**\"]\n\
             exclude = [\"**/vendor/**\"]\n\
             db = \"build/graph.redb\"\n\
             \n[tracks]\n\
             java = false\n",
        )
        .expect("parses");
        assert_eq!(config.include, ["src/**"]);
        assert_eq!(config.exclude, ["**/vendor/**"]);
        assert_eq!(config.db, Some(PathBuf::from("build/graph.redb")));
        assert!(!config.track_enabled("java"));
        // Unmentioned tracks are untouched: the table takes away, never adds.
        assert!(config.track_enabled("go"));
    }

    #[test]
    fn an_unknown_top_level_key_is_refused_by_name() {
        let e = Config::parse("includ = [\"src\"]\n").expect_err("typo is refused");
        assert!(e.contains("unknown key `includ`"), "{e}");
        assert!(
            e.contains("include"),
            "the message lists the real keys: {e}"
        );
    }

    #[test]
    fn an_unknown_track_is_refused_by_name() {
        let e = Config::parse("[tracks]\nfortran = false\n").expect_err("refused");
        assert!(e.contains("unknown key `tracks.fortran`"), "{e}");
        assert!(e.contains("go"), "the message lists the real tracks: {e}");
    }

    #[test]
    fn a_track_this_build_lacks_cannot_be_switched_on() {
        let table: toml::Table = "cobol = true\n".parse().expect("parses");
        let e = tracks(&table, &[("cobol", false)]).expect_err("refused");
        assert!(e.contains("cannot switch on a track"), "{e}");
        // …and switching the same absent track off is fine: it says nothing
        // the build does not already do.
        let table: toml::Table = "cobol = false\n".parse().expect("parses");
        assert_eq!(
            tracks(&table, &[("cobol", false)]).expect("accepted"),
            BTreeMap::from([("cobol".to_string(), false)]),
        );
        // A live track may be named either way.
        let table: toml::Table = "go = true\n".parse().expect("parses");
        assert_eq!(
            tracks(&table, &[("go", true)]).expect("accepted"),
            BTreeMap::from([("go".to_string(), true)]),
        );
    }

    #[test]
    fn a_key_of_the_wrong_type_names_itself() {
        let e = Config::parse("include = \"src/**\"\n").expect_err("refused");
        assert!(e.contains("`include` must be an array of strings"), "{e}");
        let e = Config::parse("include = [1]\n").expect_err("refused");
        assert!(e.contains("`include` must hold strings"), "{e}");
        let e = Config::parse("db = 7\n").expect_err("refused");
        assert!(e.contains("`db` must be a string"), "{e}");
        let e = Config::parse("tracks = 7\n").expect_err("refused");
        assert!(e.contains("`tracks` must be a table"), "{e}");
        let e = Config::parse("[tracks]\ngo = \"off\"\n").expect_err("refused");
        assert!(e.contains("`tracks.go` must be a boolean"), "{e}");
    }

    #[test]
    fn malformed_toml_is_an_error_and_not_the_defaults() {
        let e = Config::parse("include = [\n").expect_err("refused");
        assert!(!e.is_empty(), "the parser's own message is passed through");
    }

    #[test]
    fn an_empty_glob_or_db_is_refused_rather_than_matching_everything() {
        assert!(Config::parse("include = [\"\"]\n").is_err());
        assert!(Config::parse("db = \"\"\n").is_err());
    }

    #[test]
    fn no_globs_is_the_walks_own_default() {
        let filter = Config::default().filter(Path::new("/repo")).expect("built");
        assert!(filter.is_none());
    }

    #[test]
    fn the_db_path_is_relative_to_the_repository() {
        let config = Config::parse("db = \"build/graph.redb\"\n").expect("parses");
        assert_eq!(
            config.db_path(Path::new("/repo")).expect("inside the root"),
            Some(PathBuf::from("/repo/build/graph.redb")),
        );
        assert_eq!(
            Config::default()
                .db_path(Path::new("/repo"))
                .expect("no db"),
            None,
        );
        // `.` and a `..` that comes back are spelling, not escape, and the
        // path that is used is the normalised one — so no component of it can
        // be a link that resolves somewhere else.
        let config = Config::parse("db = \"./a/../build/graph.redb\"\n").expect("parses");
        assert_eq!(
            config.db_path(Path::new("/repo")).expect("inside the root"),
            Some(PathBuf::from("/repo/build/graph.redb")),
        );
    }

    #[test]
    fn a_db_that_leaves_the_repository_is_refused_and_says_which_path_may() {
        for value in [
            "/tmp/graph.redb",
            "../graph.redb",
            "a/../../graph.redb",
            ".",
        ] {
            let config = Config::parse(&format!("db = \"{value}\"\n")).expect("parses");
            let e = config
                .db_path(Path::new("/repo"))
                .expect_err("a db outside the repository is refused");
            assert!(e.contains(DB_OUTSIDE_ROOT), "{value}: {e}");
            // The refusal is only useful if it says what *is* allowed to point
            // outside the tree, because that is the thing the reader wants.
            assert!(e.contains("--db"), "{value}: {e}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_db_under_a_symlinked_directory_is_refused_though_it_reads_as_inside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).expect("mkdir repo");
        std::fs::create_dir_all(&outside).expect("mkdir outside");
        std::os::unix::fs::symlink(&outside, root.join("out")).expect("symlink");

        let config = Config::parse("db = \"out/graph.redb\"\n").expect("parses");
        let e = config
            .db_path(&root)
            .expect_err("a linked parent is not containment");
        assert!(e.contains(DB_OUTSIDE_ROOT), "{e}");

        // …and a real directory of the same shape is fine, so the check is
        // about where the path resolves and not about having subdirectories.
        std::fs::create_dir_all(root.join("build")).expect("mkdir build");
        let config = Config::parse("db = \"build/graph.redb\"\n").expect("parses");
        assert_eq!(
            config.db_path(&root).expect("inside the root"),
            Some(root.join("build/graph.redb")),
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_db_that_is_a_link_with_nothing_on_the_other_end_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).expect("mkdir repo");
        std::fs::create_dir_all(&outside).expect("mkdir outside");
        // `canonicalize` fails on this and `symlink_metadata` does not, which
        // is the whole difference between "not there yet" and "there, and
        // pointing somewhere this check cannot see".
        std::os::unix::fs::symlink(outside.join("planted.redb"), root.join("graph.redb"))
            .expect("symlink");

        let config = Config::parse("db = \"graph.redb\"\n").expect("parses");
        let e = config
            .db_path(&root)
            .expect_err("a link to nothing is not containment");
        assert!(e.contains(DB_OUTSIDE_ROOT), "{e}");

        // A link to nothing *inside* the root is refused by the same rule and
        // for the same reason: where a link lands is not knowable from a
        // target that is not there, and this function only ever answers what
        // it can show.
        std::os::unix::fs::symlink(root.join("later.redb"), root.join("inner.redb"))
            .expect("symlink");
        let config = Config::parse("db = \"inner.redb\"\n").expect("parses");
        assert!(config.db_path(&root).is_err(), "an unresolvable component");

        // …and an ordinary path that simply does not exist yet is still fine,
        // because that is the normal case: the store is created by the scan.
        let config = Config::parse("db = \"build/graph.redb\"\n").expect("parses");
        assert_eq!(
            config.db_path(&root).expect("inside the root"),
            Some(root.join("build/graph.redb")),
        );
    }

    #[test]
    fn a_broken_glob_is_reported_against_the_glob_that_broke() {
        let e = FileFilter::new(Path::new("/repo"), &["a[".to_string()], &[])
            .expect_err("an unclosed class is not a glob");
        assert!(e.contains("include glob `a[`"), "{e}");
    }

    #[test]
    fn every_registered_track_name_is_accepted() {
        // The `[tracks]` validator and the registry cannot drift: if a track
        // is renamed, a config naming the old name must start failing.
        for (name, _) in live_tracks() {
            let text = format!("[tracks]\n{name} = false\n");
            let config = Config::parse(&text).expect("a registered track is a valid key");
            assert!(!config.track_enabled(name));
        }
    }
}
