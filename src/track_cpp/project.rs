//! Phase 0 for C++: where an `#include` starts looking, and what this
//! repository publishes there that this build does not parse.
//!
//! C++ is the first language in this repository with **no dependency
//! manifest at all**. Go has `go.mod`, Java `pom.xml`, JavaScript
//! `package.json`, Python `pyproject.toml`, Ruby a gemspec; C++ has the
//! include path, and the include path is decided by the build system rather
//! than by the language. So this module answers one question — *which
//! directories is a `#include` resolved against* — and it answers it from the
//! **tree layout**, not from the vendored `CMakeLists.txt`.
//!
//! # Why the layout and not the CMake files
//!
//! There is no CMake grammar in this build. ast-grep 0.44.1 ships no
//! `SupportLang` for it, so reading `CMakeLists.txt` would mean a regular
//! expression over a Turing-complete build language, with variable expansion
//! (`${PROJECT_SOURCE_DIR}`) and generator expressions
//! (`$<BUILD_INTERFACE:…>`) approximated rather than evaluated. That is the
//! precise thing the Ruby track refused when it parsed a gemspec *as Ruby*
//! instead of pattern-matching the same bytes, and the reason holds harder
//! here: a build file states the include path conditionally, per target, and
//! a scan that guessed wrong would move every number the gate reports.
//!
//! The tree states the same fact in a form this build can verify: **a
//! directory named `include` at the repository root is an include root.** On
//! the measured corpus the vendored build files agree with it independently —
//! `CMakeLists.txt:243-246` says
//! `target_include_directories(… $<BUILD_INTERFACE:${PROJECT_SOURCE_DIR}/include>)`
//! and `test/CMakeLists.txt:5-6` repeats it — so the convention and the
//! manifest name the same directory, and the convention is the one this build
//! can read without inventing a parser.
//!
//! **The honest limit:** a project whose include root is spelled some other
//! way, or added only by a `-I` flag no file in the tree carries, has an
//! include root this build does not know. Its includes then miss with a
//! reason rather than resolve by accident, which is the direction a wrong
//! answer must fail in.
//!
//! # The second fact: what sits on the include path unparsed
//!
//! `.h` is not an extension this build claims, so a header named by an
//! `#include` may be a real file in this repository that no scan reads. That
//! is not the same fact as "the target is outside this repository", and
//! collapsing the two would launder an in-repository header into `External`,
//! where it would sit outside both terms of the resolution rate. So phase 0
//! lists the files under each include root that this build does **not** parse,
//! and the resolver uses that list for exactly one decision — see
//! [`crate::track_cpp::resolve`].
//!
//! No file is executed and no network call is made, here or anywhere.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::lang::{Language, LayoutError};
use crate::track_cpp::lang::CppLang;

/// The directory a C++ project conventionally publishes its headers in, and
/// the only include root this build derives from a tree.
pub const INCLUDE_DIR: &str = "include";

/// How deep phase 0 descends into an include root.
///
/// A bound rather than a cycle check: `DirEntry::file_type` does not follow
/// symlinks, so a symlinked directory is never descended, and a real tree
/// nested deeper than this is a shape no measurement covers.
const MAX_DEPTH: usize = 32;

/// What the C++ resolver needs to know about the project as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CppProject {
    /// Repository-relative include roots, in search order.
    pub include_roots: Vec<String>,
    /// Repository-relative paths under an include root that this build does
    /// not parse — `.h` headers, most of all.
    ///
    /// Sorted, because it is fingerprinted: a set whose order depended on the
    /// filesystem would wipe the store on every scan.
    pub unparsed: BTreeSet<String>,
}

impl CppProject {
    /// Everything this config decides, as bytes.
    ///
    /// Both fields are in it. The roots root every candidate an `#include`
    /// builds, so a scan under a different set describes a different graph.
    /// The unparsed listing is this language's *manifest*: with no file that
    /// declares a dependency, what sits on the include path is the only thing
    /// that says whether a header comes from this repository or from the
    /// toolchain, and a header appearing there changes the answer for
    /// references in files nobody edited.
    ///
    /// The cost is stated rather than hidden: adding one unparsed header
    /// under an include root re-scans this language's half of the store. That
    /// is what a language with no manifest buys — the alternative is a warm
    /// scan that disagrees with a cold one.
    pub fn digest(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for root in &self.include_roots {
            out.extend_from_slice(root.as_bytes());
            out.push(0);
        }
        out.push(b'\n');
        for path in &self.unparsed {
            out.extend_from_slice(path.as_bytes());
            out.push(0);
        }
        out
    }
}

/// Whether this build parses a file with this repository-relative path.
pub fn parsed(rel_path: &str) -> bool {
    Path::new(rel_path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| CppLang::extensions().contains(&ext))
}

/// Work out the include roots and what sits on them unparsed.
///
/// Total: a repository with no `include/` directory has no include roots, and
/// its angled includes then resolve against nothing — which is the honest
/// answer, not an error.
pub fn layout(root: &Path) -> Result<CppProject, LayoutError> {
    let mut project = CppProject::default();
    let candidate = root.join(INCLUDE_DIR);
    if candidate.is_dir() {
        project.include_roots.push(INCLUDE_DIR.to_string());
        collect(&candidate, INCLUDE_DIR, 0, &mut project.unparsed).map_err(|e| LayoutError {
            message: format!("reading {INCLUDE_DIR}/: {e}"),
        })?;
    }
    Ok(project)
}

/// Every file under `dir` this build does not parse, as repository-relative
/// paths.
fn collect(
    dir: &Path,
    rel: &str,
    depth: usize,
    out: &mut BTreeSet<String>,
) -> Result<(), std::io::Error> {
    if depth >= MAX_DEPTH {
        return Ok(());
    }
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().replace('\\', "/");
        let child = format!("{rel}/{name}");
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect(&entry.path(), &child, depth + 1, out)?;
        } else if file_type.is_file() && !parsed(&child) {
            out.insert(child);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, body).expect("write");
    }

    #[test]
    fn a_root_include_directory_is_the_include_root() {
        let dir = tempfile::tempdir().expect("scratch");
        write(dir.path(), "include/fmt/format.h", "");
        write(dir.path(), "include/fmt/base.hpp", "");
        write(dir.path(), "src/format.cc", "");
        let project = layout(dir.path()).expect("layout");
        assert_eq!(project.include_roots, ["include"]);
        // `.h` is not parsed, so it is listed; `.hpp` is, so it is not — the
        // walk already mints a node for it and the resolver probes that.
        assert_eq!(
            project.unparsed.iter().cloned().collect::<Vec<_>>(),
            ["include/fmt/format.h"],
        );
    }

    #[test]
    fn a_tree_with_no_include_directory_has_no_roots() {
        let dir = tempfile::tempdir().expect("scratch");
        write(dir.path(), "src/main.cc", "");
        let project = layout(dir.path()).expect("layout");
        assert!(project.include_roots.is_empty());
        assert!(project.unparsed.is_empty());
    }

    #[test]
    fn the_digest_covers_the_roots_and_what_sits_on_them() {
        let dir = tempfile::tempdir().expect("scratch");
        write(dir.path(), "include/a.h", "");
        let before = layout(dir.path()).expect("layout").digest();
        write(dir.path(), "include/b.h", "");
        let after = layout(dir.path()).expect("layout").digest();
        assert_ne!(
            before, after,
            "a header appearing on the include path changes what unedited files resolve to",
        );
        // A parsed file is tracked by the walk and by the candidate index, so
        // it must not also churn the fence.
        write(dir.path(), "include/c.hpp", "");
        assert_eq!(after, layout(dir.path()).expect("layout").digest());
    }
}
