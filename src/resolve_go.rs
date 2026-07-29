//! The Go resolver: all cross-file linking for Go. Never drops.
//!
//! Probes a [`SymbolProbe`] (the store in production, a `HashSet` in tests)
//! with content-addressed candidate FQNs and classifies every reference
//! into exactly one [`Outcome`]. Every candidate probed — hit or miss — is
//! returned for the invalidation index.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::extract_go::GoHeader;
use crate::lang::{
    Entry, FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe,
};
use crate::model::{
    DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, Reference, TargetRoot, node_id,
};
use crate::{Outcome, UnresolvedReason};

/// Go's universe-scope builtin functions.
const GO_BUILTINS: &[&str] = &[
    "append", "cap", "clear", "close", "complex", "copy", "delete", "imag", "len", "make", "max",
    "min", "new", "panic", "print", "println", "real", "recover",
];

/// The universe-scope names a Go *type position* can write.
///
/// A separate list from [`GO_BUILTINS`] and not a merge of it, because the two
/// answer different questions and one of them is a trap: `min` is a builtin
/// function and is not a type, and a `type_identifier` reading `min` would be
/// a type this repository failed to find rather than a link to the universe.
///
/// Both are probed at the same point — last, after the package's own
/// declarations and its internal dot-imports — because the universe is the
/// outermost scope and a package may legally declare `type rune …`.
/// `any` and `comparable` are here: they are predeclared names, whatever
/// `any`'s alias-hood says about its definition. So is `nil`, which is not a
/// type but is the one other universe name a type position may hold — a
/// TypeSwitchCase may be written `case nil:`, and the grammar hands that over
/// as a `type_identifier` like any other. Answering it from the universe is
/// what stops a legal switch arm from being filed as arthron's own bug.
///
/// The alternative was to leave them out of the extractor the way
/// `rules/typescript.yml` leaves out `predefined_type`. TypeScript can: its
/// grammar has a distinct node for `string` and `void`. Go's does not — `int`
/// is a `type_identifier` and so is `Node` — so suppressing them by *name* in
/// the extractor would silently delete a real reference from any package that
/// declares its own, and the reference would be gone from the count rather
/// than reported outside it.
const GO_UNIVERSE_TYPES: &[&str] = &[
    "any",
    "bool",
    "byte",
    "comparable",
    "complex64",
    "complex128",
    "error",
    "float32",
    "float64",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "nil",
    "rune",
    "string",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
];

/// Go, as the shared driver sees it.
pub struct GoLang;

impl Language for GoLang {
    const LANG: Lang = Lang::Go;
    const DOMAIN: Domain = Domain::Go;

    fn extensions() -> &'static [&'static str] {
        &["go"]
    }

    fn skip_dirs() -> &'static [&'static str] {
        // Vendored dependencies and fixture trees are not this module's
        // source. A directory governed by a *nested* `go.mod` is excluded
        // too, but that is a manifest fact, so it lives in `owns_file`.
        &["vendor", "testdata"]
    }

    type Header = GoHeader;
    type Scope = FileScope;
    type Config = GoModule;
}

/// Facts parsed from a `go.mod` file, plus what the scan learned about the
/// packages around it.
///
/// This is `GoLang`'s configuration: the driver moves it between phases and
/// never inspects it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoModule {
    /// The module path from the `module` directive.
    pub path: String,
    /// Module paths from `require` directives.
    pub requires: Vec<String>,
    /// Import path → declared package name, for every package the store or
    /// this event knows. An unaliased import binds the *imported* package's
    /// declared name, which is a fact in that package's source rather than
    /// in the path, so it is not per-file derivable.
    pub package_names: HashMap<String, String>,
    /// Repo-relative directories that declare their own `go.mod`. Their
    /// files belong to another module, so they are not this scan's.
    pub nested_modules: Vec<String>,
}

/// Parse a `go.mod` source. `None` when there is no `module` directive.
pub fn parse_go_mod(src: &str) -> Option<GoModule> {
    let mut path = None;
    let mut requires = Vec::new();
    let mut in_require_block = false;
    for line in src.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        // go.mod separates a directive from its argument with any run of
        // whitespace — gofmt writes tabs — so tokenise rather than match a
        // single-space prefix.
        let mut tokens = line.split_whitespace();
        let Some(first) = tokens.next() else {
            continue; // blank, or comment-only
        };
        let second = tokens.next();
        if in_require_block {
            if first == ")" {
                in_require_block = false;
            } else {
                requires.push(first.to_string());
            }
            continue;
        }
        match (first, second) {
            ("module", Some(module_path)) => path = Some(module_path.to_string()),
            ("require", Some("(")) => in_require_block = true,
            ("require", Some(dep)) => requires.push(dep.to_string()),
            _ => {}
        }
    }
    Some(GoModule {
        path: path?,
        requires,
        package_names: HashMap::new(),
        nested_modules: Vec::new(),
    })
}

/// The name of Go's package initialiser, `func init()`.
///
/// Go forbids naming one: it cannot be called, assigned, or referred to at
/// all, and a package may declare any number of them. A node is a thing a
/// reference can name, so this is not one — it gets no definition node, and
/// the references inside it belong to the package, exactly as a package-level
/// variable's initialiser does. Giving it a node instead collapses every
/// `init` in a package into a single `{pkg}.init` identity, which is one
/// definition nothing can name standing in for many distinct bodies.
pub const INIT_FUNC: &str = "init";

/// Whether a definition is Go's package initialiser. See [`INIT_FUNC`].
///
/// A *method* called `init` is an ordinary method — `x.init()` names it — so
/// only a plain function declaration qualifies.
pub fn is_init_func(def: &Definition) -> bool {
    matches!(def.kind, DefKind::Function) && def.name == INIT_FUNC
}

/// Per-file resolution scope: the package path plus the import table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileScope {
    /// Import path of the package this file belongs to.
    pub pkg_path: String,
    /// Import name (alias or last path segment) → import path.
    /// Blank (`_`) and dot (`.`) imports are excluded.
    pub imports: HashMap<String, String>,
    /// Paths dot-imported into this file's scope.
    pub dot_imports: Vec<String>,
    /// Byte offsets of this file's composite-literal keys, carried through
    /// from [`GoHeader::literal_keys`] — the one thing about a
    /// [`RefKind::FieldAccess`] that only the syntax could say.
    pub literal_keys: HashSet<u32>,
}

/// Whether a path segment is a Go major-version marker (`v2`, `v3`, …).
fn is_version_segment(segment: &str) -> bool {
    match segment.strip_prefix('v') {
        Some(digits) => !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// The name an unaliased import path *suggests* it binds to.
///
/// Go binds an unaliased import to the imported package's **declared** name,
/// which lives in that package's source and not in the import path. This is
/// therefore a heuristic, and the answer only when the declaration is out of
/// reach: an external package, whose source is never indexed, or a package in
/// this repository that no scan has reached yet. Module version suffixes are
/// how the last segment usually lies, so strip them — `gopkg.in/yaml.v3`
/// binds `yaml`, `github.com/foo/bar/v2` binds `bar`, and a plain path binds
/// its last segment.
///
/// For a package in this repository the declared name is a fact the extractor
/// already records ([`GoHeader::package`]); [`file_scope`] prefers it, given
/// the package names carried on [`GoModule`]. It doubles as the name a
/// directory's package is assumed to use when nothing in it has been indexed.
pub fn import_binding(path: &str) -> &str {
    let mut segments = path.rsplit('/');
    let last = segments.next().unwrap_or(path);
    if is_version_segment(last) {
        // `github.com/foo/bar/v2` → `bar`; a bare `v2` has nothing before it.
        return segments.next().filter(|s| !s.is_empty()).unwrap_or(last);
    }
    match last.rsplit_once('.') {
        // `gopkg.in/yaml.v3` → `yaml`
        Some((name, version)) if !name.is_empty() && is_version_segment(version) => name,
        _ => last,
    }
}

/// The package name a directory is known to use: what a file in it
/// declares, or — when no scan has reached one — what its import path
/// suggests.
///
/// Telling `package foo_test` in the directory of package `foo` from a
/// directory whose package genuinely is `foo_test` is what needs this.
pub fn dir_package_name<'a>(names: &'a HashMap<String, String>, pkg_path: &'a str) -> &'a str {
    names
        .get(pkg_path)
        .map_or_else(|| import_binding(pkg_path), String::as_str)
}

/// Whether a file declares an *external test package*.
///
/// `package foo_test` in the directory of package `foo` is a second package
/// sharing one directory — Go's only such case. Two facts decide it, not
/// one. The file must *be* a test file: `package foo_test` in a
/// non-`_test.go` file is not an external test package at all, because the
/// Go toolchain rejects that directory outright, so the suffix alone was
/// never the rule. And the declared name must differ from the name that
/// directory's package uses, which is what keeps a directory whose package
/// genuinely is called `foo_test` out of this.
pub fn is_external_test_package(rel_path: &str, declared: &str, dir_pkg_name: &str) -> bool {
    rel_path.ends_with("_test.go") && declared.ends_with("_test") && declared != dir_pkg_name
}

/// The import path a file's definitions and same-package candidates live
/// under: its directory's package path, unless it is an external test file.
///
/// An external test package may not be imported by anything, and its
/// definitions must not sit in the production package's namespace, where a
/// same-package candidate from a production file would wrongly hit one. It
/// gets `{dir_pkg_path}!test` — its own package, with its own node. It
/// reaches the package under test the ordinary way, through the explicit
/// import it has to write anyway.
///
/// The marker is `!`, which the Go module-path grammar forbids in an import
/// path, so `{dir}!test` is an identity no real directory can claim. A
/// `_test` suffix could: a directory literally named `foo_test` beside the
/// external test package of `foo` used to share one namespace with it, and a
/// same-package candidate could cross between the two.
///
/// It is `!` and not the `#` that separates a container from its members,
/// because `#` is already spoken for: `{dir}#test` would be exactly the FQN
/// of a definition named `test` in package `{dir}`, and `func test()` is an
/// ordinary unexported helper. Two reserved characters, one job each.
pub fn package_path_for_file(
    rel_path: &str,
    dir_pkg_path: &str,
    declared: Option<&str>,
    dir_pkg_name: &str,
) -> String {
    match declared {
        Some(name) if is_external_test_package(rel_path, name, dir_pkg_name) => {
            format!("{dir_pkg_path}!test")
        }
        _ => dir_pkg_path.to_string(),
    }
}

/// Build a [`FileScope`] from a file's header.
///
/// `pkg_path` is the package the file's own definitions live in — see
/// [`package_path_for_file`], which needs to know what name the directory's
/// package uses.
///
/// `cfg.package_names` maps import path → declared package name for every
/// package this repository has indexed. Binding an unaliased import needs a
/// fact out of the *imported* package's source, so anything missing from it
/// falls back to [`import_binding`].
pub fn file_scope(
    resolver: &GoResolver,
    cfg: &GoModule,
    pkg_path: String,
    header: &GoHeader,
) -> FileScope {
    let mut imports = HashMap::new();
    let mut dot_imports = Vec::new();
    for imp in &header.imports {
        match imp.alias.as_deref() {
            Some("_") => {}
            Some(".") => dot_imports.push(imp.path.clone()),
            Some(alias) => {
                imports.insert(alias.to_string(), imp.path.clone());
            }
            None => {
                let declared = cfg
                    .package_names
                    .get(&imp.path)
                    .filter(|_| resolver.is_internal(cfg, &imp.path))
                    .map(String::as_str);
                let bound = declared.unwrap_or_else(|| import_binding(&imp.path));
                imports.insert(bound.to_string(), imp.path.clone());
            }
        }
    }
    FileScope {
        pkg_path,
        imports,
        dot_imports,
        literal_keys: header.literal_keys.iter().copied().collect(),
    }
}

/// All Go linking decisions. Stateless: every fact it needs rides on the
/// [`GoModule`] configuration the driver hands it.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoResolver;

impl GoResolver {
    /// The import path of the package in a directory (repo-relative, `/`
    /// separated, empty string for the module root).
    pub fn package_path(&self, cfg: &GoModule, rel_dir: &str) -> String {
        if rel_dir.is_empty() {
            cfg.path.clone()
        } else {
            format!("{}/{}", cfg.path, rel_dir)
        }
    }

    /// The package a file's definitions and same-package candidates belong
    /// to: its directory's, except for an external test file, which is its
    /// own package.
    pub fn file_package_path(&self, cfg: &GoModule, header: &GoHeader) -> String {
        let rel_dir = match header.rel_path.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        };
        let dir_pkg = self.package_path(cfg, rel_dir);
        let dir_name = dir_package_name(&cfg.package_names, &dir_pkg);
        package_path_for_file(
            &header.rel_path,
            &dir_pkg,
            header.package.as_deref(),
            dir_name,
        )
    }

    fn is_internal(&self, cfg: &GoModule, import_path: &str) -> bool {
        import_path == cfg.path || import_path.starts_with(&format!("{}/", cfg.path))
    }

    fn is_stdlib(import_path: &str) -> bool {
        !import_path
            .split('/')
            .next()
            .unwrap_or(import_path)
            .contains('.')
    }

    /// Classify an import reference.
    pub fn resolve_import(
        &self,
        cfg: &GoModule,
        path: &str,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        if self.is_internal(cfg, path) {
            let id = node_id(Domain::Go, path);
            let outcome = if probe.probe(&id).is_some() {
                Outcome::Resolved(id)
            } else {
                Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
            };
            return Resolution {
                outcome,
                candidates: vec![id],
            };
        }
        if Self::is_stdlib(path) {
            return Resolution {
                outcome: Outcome::External(format!("std:{path}")),
                candidates: vec![],
            };
        }
        if cfg
            .requires
            .iter()
            .any(|r| path == r || path.starts_with(&format!("{r}/")))
        {
            return Resolution {
                outcome: Outcome::External(path.to_string()),
                candidates: vec![],
            };
        }
        Resolution {
            outcome: Outcome::Unresolved(UnresolvedReason::UnknownPackage),
            candidates: vec![],
        }
    }

    /// `t.M()` inside `func (t *T) …` — Go's spelling of `this.M()`.
    ///
    /// The receiver's type is written in the method's own signature, so this
    /// is a declared-type lookup and not inference: the enclosing definition
    /// already carries the receiver type name, and a method's FQN is
    /// `{pkg}#{Type}.{name}`. Java's `this.m()` (X-02) and Python's `self.m()`
    /// (E-01) resolve the same shape the same way; before this the Go track
    /// alone called it a local binding and left it outside both terms of the
    /// rate, which made a Go rate and a Java rate two different measurements.
    ///
    /// A miss is [`UnresolvedReason::NeedsReceiverType`] and not
    /// [`UnresolvedReason::NoMatchingDefinition`]: the lookup table is *not*
    /// complete here. Go promotes the members of an embedded field into the
    /// outer type's method set, and this track indexes neither embedding nor
    /// struct fields, so a name absent from `{pkg}#{Type}` may still be a
    /// perfectly ordinary member of it. Blaming the repository for that would
    /// put arthron's own missing work in the bucket reserved for its bugs.
    pub fn resolve_receiver(
        &self,
        scope: &FileScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let unresolved = |reason| Resolution {
            outcome: Outcome::Unresolved(reason),
            candidates: vec![],
        };
        // `t.a.b()`: the receiver of `b` is the field `a`, whose type this
        // file does not state. `NeedsReceiverType` would claim a declared
        // type that was never written, and `LocalBinding` would claim the
        // receiver is not a node — it is the one thing here that certainly
        // is one.
        let [member] = r.target.segments.as_slice() else {
            return unresolved(UnresolvedReason::NeedsTypeInference);
        };
        // The encloser of a method is `[ReceiverType, MethodName]`. Anything
        // else means the receiver's type name could not be read.
        let owner = match r.enclosing.as_ref().map(|e| e.path.as_slice()) {
            Some([owner, _]) if !owner.is_empty() => owner,
            _ => return unresolved(UnresolvedReason::NeedsReceiverType),
        };
        let id = node_id(Domain::Go, &format!("{}#{owner}.{member}", scope.pkg_path));
        let outcome = if probe.probe(&id).is_some() {
            Outcome::Resolved(id)
        } else {
            Outcome::Unresolved(UnresolvedReason::NeedsReceiverType)
        };
        Resolution {
            outcome,
            candidates: vec![id],
        }
    }

    /// Classify a field access: `pkg.Name`, `t.field`, `T.Method`, and the
    /// `Field` in `T{Field: v}`.
    ///
    /// A call and a read share most shapes, and this shares
    /// [`Self::resolve_call`]'s answers on those: a two-segment `pkg.Name` is
    /// handed straight to it, a receiver root goes to
    /// [`Self::resolve_receiver`], and a root bound by an enclosing block
    /// never reaches here at all.
    ///
    /// One shape they answer differently, and the doc says so rather than
    /// claiming a symmetry that is not there. A *three*-segment path rooted
    /// in an import — `pkg.T.Member`, which is what `pkg.T{Member: v}` and
    /// `pkg.Var.Field` both reduce to — is [`Outcome::External`] here and
    /// [`UnresolvedReason::NeedsTypeInference`] in [`Self::resolve_call`],
    /// which files every path longer than a `QualifiedIdent` under one
    /// answer. Reading it as `External` is what a literal key on a
    /// dependency's type is — a reference *into* that dependency, filed the
    /// way every other reference into one is — and a selector read of the
    /// same shape rides along. Making the two agree moves a measured number
    /// in one direction or the other and is its own change, with its own
    /// attribution.
    ///
    /// What it adds is the shape a call does not have: a member named on an
    /// owner that is *written at the site*. `T.Method` is Go's method
    /// expression, `T{Field: v}` states its own type, and `pkg.T{Field: v}`
    /// states it through an import — in all three the owner is a type name,
    /// not a value whose type has to be inferred. So the member is probed
    /// under that owner, and only a miss needs a reason.
    ///
    /// The reason on a miss is decided by *what the owner is*, which the graph
    /// already knows: an owner that is a type in this repository makes the
    /// miss [`UnresolvedReason::NeedsReceiverType`] — the declared type is
    /// known and the member is not indexed, exactly as for a receiver, because
    /// this track indexes neither Go embedding nor struct fields. An owner
    /// that is anything else — a package-level `var`, a name this build has
    /// never seen — leaves the site's type genuinely unstated, which is
    /// [`UnresolvedReason::NeedsTypeInference`].
    ///
    /// Deliberately not applied to a `Call`. `T.Method(x)` is the same shape
    /// in callee position and would resolve the same way, but every such row
    /// already exists with an answer, and moving one is a change to a measured
    /// number that this piece of work is not making. It is its own change,
    /// with its own attribution.
    ///
    /// # A composite-literal key never links
    ///
    /// A site [`FileScope::literal_keys`] marks is the `Field` in
    /// `T{Field: v}`, and the member probe is skipped for it entirely: it can
    /// only ever find the wrong node. A Go struct field is not a node in this
    /// build, and Go forbids a type from declaring a method and a field of one
    /// name, so `T.Field` existing in the table proves `T` is *not* a struct —
    /// a named map, slice or array type, whose literal key is an index
    /// expression rather than a member name and whose same-named method the
    /// key does not name. Linking it would emit a wrong edge, which is
    /// strictly worse than an unresolved reference. Skipping the probe costs
    /// nothing a compiling corpus could have earned.
    pub fn resolve_field(
        &self,
        cfg: &GoModule,
        r: &Reference,
        scope: &FileScope,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let unresolved = |reason| Resolution {
            outcome: Outcome::Unresolved(reason),
            candidates: vec![],
        };
        if matches!(r.target.root, TargetRoot::This { .. }) {
            return self.resolve_receiver(scope, r, probe);
        }
        if r.target.root != TargetRoot::Name {
            // `f().x`, `m[k].x`. No name is written, so no lookup was even
            // attempted — and that is a different fact from a name whose type
            // is unstated, which is why the taxonomy carries both.
            return unresolved(UnresolvedReason::NeedsExpressionType);
        }
        // Split the path into the package it is rooted in, the owner, and the
        // member. Only an import can supply a package here: a bare `a.b` is
        // rooted in this file's own package.
        let (pkg, owner, member) = match r.target.segments.as_slice() {
            [owner, member] => (scope.pkg_path.clone(), owner, member),
            [qualifier, owner, member] => match scope.imports.get(qualifier) {
                Some(path) if self.is_internal(cfg, path) => (path.clone(), owner, member),
                Some(path) => {
                    return Resolution {
                        outcome: Outcome::External(path.clone()),
                        candidates: vec![],
                    };
                }
                // `a.b.c` where `a` is a package-level value: its type is not
                // stated at the site.
                None => return unresolved(UnresolvedReason::NeedsTypeInference),
            },
            // A bare name, or a chain longer than one owner and one member.
            _ => return unresolved(UnresolvedReason::NeedsTypeInference),
        };
        // A two-segment path may still be `pkg.Name` — a package member read
        // through an import — which is the same lookup a call makes and must
        // give the same answer.
        if r.target.segments.len() == 2 && scope.imports.contains_key(owner) {
            return self.resolve_call(cfg, r, scope, probe);
        }
        let mut candidates = Vec::new();
        // A composite-literal key is the one shape that must not be probed:
        // see the type-level note above. Every other site asks the member
        // question the shape states.
        if !scope.literal_keys.contains(&r.span.byte_start) {
            let id = node_id(Domain::Go, &format!("{pkg}#{owner}.{member}"));
            candidates.push(id);
            if probe.probe(&id).is_some() {
                return Resolution {
                    outcome: Outcome::Resolved(id),
                    candidates,
                };
            }
        }
        // The member is not indexed under that owner. Whether that is a gap in
        // this track or a type nobody stated is decided by the owner itself.
        let owner_id = node_id(Domain::Go, &format!("{pkg}#{owner}"));
        candidates.push(owner_id);
        let reason = match probe.probe(&owner_id) {
            Some(Entry::Definition {
                kind: DefKind::Type,
                ..
            }) => UnresolvedReason::NeedsReceiverType,
            _ => UnresolvedReason::NeedsTypeInference,
        };
        Resolution {
            outcome: Outcome::Unresolved(reason),
            candidates,
        }
    }

    /// Classify a call reference against a file's scope.
    ///
    /// The dispatch is on `(root, segments.len())`. A name root with one
    /// segment is a package-block lookup; with two it is Go's
    /// `QualifiedIdent`, read against the import table. A `this` root is the
    /// receiver, handled by [`Self::resolve_receiver`]. Anything else — a
    /// longer chain, or a root that is not a name — needs the type of an
    /// expression, which this resolver does not compute.
    pub fn resolve_call(
        &self,
        cfg: &GoModule,
        r: &Reference,
        scope: &FileScope,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let needs_inference = || Resolution {
            outcome: Outcome::Unresolved(UnresolvedReason::NeedsTypeInference),
            candidates: vec![],
        };
        if matches!(r.target.root, TargetRoot::This { .. }) {
            return self.resolve_receiver(scope, r, probe);
        }
        if r.target.root != TargetRoot::Name {
            return needs_inference();
        }
        match r.target.segments.as_slice() {
            [name] => {
                // Same package first, then internal dot-imports, in order.
                // Generated and probed one at a time, stopping at the first
                // hit: `candidates` must list what was probed and nothing
                // else, or the invalidation index it feeds would wake this
                // reference for edits that could not change its outcome.
                let same_pkg = format!("{}#{name}", scope.pkg_path);
                let dotted = scope
                    .dot_imports
                    .iter()
                    .filter(|dot| self.is_internal(cfg, dot))
                    .map(|dot| format!("{dot}#{name}"));
                let mut candidates = Vec::new();
                for fqn in std::iter::once(same_pkg).chain(dotted) {
                    let id = node_id(Domain::Go, &fqn);
                    candidates.push(id);
                    if probe.probe(&id).is_some() {
                        return Resolution {
                            outcome: Outcome::Resolved(id),
                            candidates,
                        };
                    }
                }
                // Nothing in scope defines the name. The universe scope is
                // the outermost one, so a predeclared name is the answer only
                // after every candidate has been probed and missed —
                // otherwise a package-level `min`, or a package-level
                // `type rune …`, could never resolve.
                //
                // Which list answers depends on what the site wrote, and the
                // grammar has already decided that: a `type_identifier` is a
                // `TypeUse` and can only name a predeclared *type*, while
                // everything reaching here from `ref-call` named a value.
                // Merging the two lists would answer `min` — a builtin
                // function — for a written type, which is not a type in any
                // Go program.
                //
                // The split leaves one shape unimproved and says so rather
                // than quietly widening: `int(x)` is a conversion the grammar
                // routes through `call_expression`, so it arrives here as a
                // `Call`, misses `GO_BUILTINS`, and is still
                // `NoMatchingDefinition`. That was its answer before type
                // uses existed and is a separate piece of work — teaching the
                // resolver which single-argument calls are conversions —
                // rather than something this list can fix by growing.
                //
                // A call written with exactly one argument is the third case,
                // and it is why this is a *list* of lists. Go writes the
                // conversion `string(b)` exactly as it writes the call `f(b)`,
                // and the grammar files both as `call_expression`, so the site
                // arrives here as a `Call` naming a predeclared *type*. It
                // missed `GO_BUILTINS` and was reported
                // `NoMatchingDefinition` — the bucket whose contract is that
                // the lookup table was complete and the name absent. The name
                // is not absent: it is in the universe block, one list over.
                // That was 123 rows on `codeiq` and 269 on `caddy`, every one
                // of them a predeclared type name, and they were the whole of
                // that bucket on both corpora. A type cannot be called, so a
                // one-argument call naming one is a conversion and nothing
                // else; arity is what separates it, and no other arity can
                // reach a type at all.
                //
                // Two names in the type list are excluded from that widening,
                // because the argument for it does not reach them. `nil` and
                // `comparable` are in [`GO_UNIVERSE_TYPES`] for the two
                // positions that write them — a `case nil:` arm and a type
                // constraint — and neither is a type a conversion can name, so
                // `nil(x)` and `comparable(x)` are not Go. Admitting them
                // would have the list claim something wider than "a type
                // cannot be called"; no compiling corpus can reach either.
                let universe: &[&[&str]] = match r.kind {
                    RefKind::TypeUse => &[GO_UNIVERSE_TYPES],
                    _ if r.argc == Some(1) && !matches!(name.as_str(), "nil" | "comparable") => {
                        &[GO_BUILTINS, GO_UNIVERSE_TYPES]
                    }
                    _ => &[GO_BUILTINS],
                };
                let outcome = if universe.iter().any(|list| list.contains(&name.as_str())) {
                    Outcome::External("go:builtin".to_string())
                } else {
                    Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
                };
                Resolution {
                    outcome,
                    candidates,
                }
            }
            [qualifier, name] => {
                match scope.imports.get(qualifier) {
                    Some(path) if self.is_internal(cfg, path) => {
                        let id = node_id(Domain::Go, &format!("{path}#{name}"));
                        let outcome = if probe.probe(&id).is_some() {
                            Outcome::Resolved(id)
                        } else {
                            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
                        };
                        Resolution {
                            outcome,
                            candidates: vec![id],
                        }
                    }
                    Some(path) => Resolution {
                        outcome: Outcome::External(path.clone()),
                        candidates: vec![],
                    },
                    // The qualifier is a variable: needs type inference.
                    None => needs_inference(),
                }
            }
            // No segments at all, or a chain longer than `QualifiedIdent`.
            _ => needs_inference(),
        }
    }
}

/// The repo-relative directories that declare their own `go.mod`,
/// considering only the directories the walk actually reached.
fn nested_module_dirs(root: &Path, files: &FileIndex) -> Vec<String> {
    let mut checked: HashMap<String, bool> = HashMap::new();
    let mut nested: BTreeSet<String> = BTreeSet::new();
    for rel in &files.files {
        let Some((dir, _)) = rel.rsplit_once('/') else {
            continue; // a file directly at the module root
        };
        let mut current = Some(dir);
        while let Some(d) = current {
            if d.is_empty() {
                break;
            }
            let has_manifest = *checked
                .entry(d.to_string())
                .or_insert_with(|| root.join(d).join("go.mod").is_file());
            if has_manifest {
                nested.insert(d.to_string());
            }
            current = d.rsplit_once('/').map(|(parent, _)| parent);
        }
    }
    nested.into_iter().collect()
}

impl Resolver<GoLang> for GoResolver {
    fn config(&self, root: &Path, files: &FileIndex) -> Result<GoModule, LayoutError> {
        let src = std::fs::read_to_string(root.join("go.mod")).map_err(|e| LayoutError {
            message: format!("reading go.mod: {e}"),
        })?;
        let mut module = parse_go_mod(&src).ok_or_else(|| LayoutError {
            message: "go.mod has no module directive".to_string(),
        })?;
        module.nested_modules = nested_module_dirs(root, files);
        Ok(module)
    }

    fn config_digest(&self, cfg: &GoModule) -> Vec<u8> {
        // Exactly the three go.mod-derived facts, each length-prefixed so no
        // pair of field values can be concatenated into another: the module
        // path roots every FQN in the graph, `requires` decides whether an
        // import is a known dependency or an unknown package, and
        // `nested_modules` decides which files this scan owns at all.
        //
        // `package_names` is deliberately absent. The driver teaches it from
        // the store as the scan proceeds, so folding it in would make the
        // fingerprint change on every scan and wipe the graph each time.
        let mut hasher = blake3::Hasher::new();
        let mut field = |bytes: &[u8]| {
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(cfg.path.as_bytes());
        field(&(cfg.requires.len() as u64).to_le_bytes());
        for require in &cfg.requires {
            field(require.as_bytes());
        }
        for nested in &cfg.nested_modules {
            field(nested.as_bytes());
        }
        hasher.finalize().as_bytes().to_vec()
    }

    fn declared_container(&self, cfg: &GoModule, header: &GoHeader) -> Option<(String, String)> {
        // Only a non-test file decides a directory's package name. A
        // `_test.go` file may declare an external test package — `package
        // foo_test` beside package `foo` — and reading that as the
        // directory's own name is exactly the confusion this prevents:
        // `is_external_test_package` asks whether the declared name differs
        // from the directory's, and it cannot be its own answer.
        if header.rel_path.ends_with("_test.go") {
            return None;
        }
        let name = header.package.as_deref().filter(|n| !n.is_empty())?;
        let rel_dir = match header.rel_path.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        };
        Some((self.package_path(cfg, rel_dir), name.to_string()))
    }

    fn learn_containers(&self, cfg: &mut GoModule, names: &HashMap<String, String>) {
        for (path, name) in names {
            cfg.package_names.insert(path.clone(), name.clone());
        }
    }

    fn owns_file(&self, cfg: &GoModule, rel_path: &str) -> bool {
        !cfg.nested_modules.iter().any(|dir| {
            rel_path.len() > dir.len()
                && rel_path.starts_with(dir.as_str())
                && rel_path.as_bytes()[dir.len()] == b'/'
        })
    }

    fn def_fqn(
        &self,
        cfg: &GoModule,
        header: &GoHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        let pkg_path = self.file_package_path(cfg, header);
        if def.kind == DefKind::Module {
            // The container itself: a Go package is named by its import path.
            return Some(Fqn::new(pkg_path));
        }
        if owner.is_empty() && is_init_func(def) {
            return None; // nothing can name it, so it is not a node
        }
        // `#` separates a container from its members, and `.` only joins
        // identifiers *within* one container — a method to its receiver
        // type. It cannot be `.` throughout: a Go import path may carry a
        // dot inside a path element (`gopkg.in/yaml.v3`, and any directory
        // someone names `p.Foo`), so `{pkg}.{name}` would give the function
        // `Foo` of package `example.com/m/p` and the package in directory
        // `p.Foo` one identity and one node, each silently overwriting the
        // other. `#` is forbidden in an import path and in an identifier
        // alike, so a definition's FQN carries exactly one and a container's
        // carries none.
        if owner.is_empty() {
            Some(Fqn::new(format!("{pkg_path}#{}", def.name)))
        } else {
            Some(Fqn::new(format!(
                "{pkg_path}#{}.{}",
                owner.join("."),
                def.name
            )))
        }
    }

    fn index_keys(&self, _cfg: &GoModule, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        // Go reaches every definition by its FQN alone: no overload sets, no
        // export aliases, no member-name keys.
        Vec::new()
    }

    fn mergeable(&self, _a: &Definition, _b: &Definition) -> bool {
        // Two Go declarations sharing an FQN are two entities, never one:
        // build-configuration-exclusive twins (`a_linux.go` and
        // `a_darwin.go` both declaring `func plat()`) are legal and distinct.
        false
    }

    fn scope(
        &self,
        cfg: &GoModule,
        file: &FileFacts<GoLang>,
        _probe: &dyn SymbolProbe,
    ) -> FileScope {
        let pkg_path = self.file_package_path(cfg, &file.header);
        file_scope(self, cfg, pkg_path, &file.header)
    }

    fn link_kinds(&self) -> &'static [RefKind] {
        // Nothing in Go has to reach a fixed point before ordinary
        // resolution can start.
        &[]
    }

    fn resolve(
        &self,
        cfg: &GoModule,
        scope: &FileScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        // Checked before any candidate is generated. A name some enclosing
        // block binds is not a node by design, so linking it would emit a
        // wrong edge — strictly worse than an unresolved reference, because
        // a miss is counted and a wrong edge is not.
        //
        // Empty candidates are contract-legal here and only here: the
        // verdict is decidable from one file, so no definition edit anywhere
        // can change it, and indexing it under a key would only wake it for
        // edits that cannot matter.
        if r.locally_bound {
            return Resolution {
                outcome: Outcome::Unresolved(UnresolvedReason::LocalBinding),
                candidates: vec![],
            };
        }
        match r.kind {
            RefKind::Import => self.resolve_import(cfg, &r.raw_target, probe),
            RefKind::FieldAccess => self.resolve_field(cfg, r, scope, probe),
            _ => self.resolve_call(cfg, r, scope, probe),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract_go::Import;
    use crate::model::{DeclSpace, DefFacets, Encloser, RefTarget, Span};

    const NOWHERE: Span = Span {
        byte_start: 0,
        byte_end: 0,
        line: 1,
    };

    fn module() -> GoModule {
        GoModule {
            path: "example.com/app".into(),
            requires: vec!["github.com/pkg/errors".into()],
            package_names: HashMap::new(),
            nested_modules: Vec::new(),
        }
    }

    fn def(kind: DefKind, name: &str, owner: &[&str]) -> Definition {
        Definition {
            kind,
            name: name.to_string(),
            owner: owner.iter().map(|s| (*s).to_string()).collect(),
            space: DeclSpace::Value,
            facets: DefFacets::default(),
            params: None,
            span: NOWHERE,
        }
    }

    fn header(rel_path: &str, package: &str, imports: Vec<Import>) -> GoHeader {
        GoHeader {
            rel_path: rel_path.to_string(),
            package: Some(package.to_string()),
            imports,
            literal_keys: Vec::new(),
        }
    }

    fn unaliased(path: &str) -> Import {
        Import {
            alias: None,
            path: path.to_string(),
            span: NOWHERE,
        }
    }

    fn named(segments: &[&str]) -> RefTarget {
        RefTarget {
            root: TargetRoot::Name,
            segments: segments.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn call(target: RefTarget) -> Reference {
        Reference {
            kind: RefKind::Call,
            space: DeclSpace::Value,
            raw_target: String::new(),
            target,
            locally_bound: false,
            argc: None,
            enclosing: None,
            span: NOWHERE,
        }
    }

    fn scope() -> FileScope {
        let mut imports = HashMap::new();
        imports.insert("util".into(), "example.com/app/util".into());
        imports.insert("errors".into(), "github.com/pkg/errors".into());
        FileScope {
            pkg_path: "example.com/app/server".into(),
            imports,
            dot_imports: vec![],
            literal_keys: HashSet::new(),
        }
    }

    #[test]
    fn go_mod_parses_module_and_requires() {
        let m = parse_go_mod(
            "module example.com/app\n\ngo 1.22\n\nrequire (\n\tgithub.com/pkg/errors v0.9.1 // indirect\n)\nrequire golang.org/x/sync v0.7.0\n",
        )
        .unwrap();
        assert_eq!(m.path, "example.com/app");
        assert_eq!(m.requires, ["github.com/pkg/errors", "golang.org/x/sync"]);
    }

    #[test]
    fn go_mod_directives_tolerate_tabs_and_runs_of_spaces() {
        // gofmt aligns go.mod with tabs, so "module\tpath" is the common
        // on-disk shape, not the exception.
        let spaced = parse_go_mod(
            "module example.com/app\n\ngo 1.22\n\nrequire (\n\tgithub.com/pkg/errors v0.9.1 // indirect\n)\nrequire golang.org/x/sync v0.7.0\n",
        )
        .expect("spaced go.mod parses");
        let tabbed = parse_go_mod(
            "module\texample.com/app\n\ngo 1.22\n\nrequire\t(\n\tgithub.com/pkg/errors\tv0.9.1 // indirect\n)\nrequire   golang.org/x/sync v0.7.0\n",
        )
        .expect("tab-separated go.mod parses");
        assert_eq!(tabbed, spaced);
    }

    #[test]
    fn init_is_not_a_node_but_a_method_called_init_is() {
        assert!(is_init_func(&def(DefKind::Function, "init", &[])));
        assert!(
            !is_init_func(&def(DefKind::Method, "init", &["Conn"])),
            "`c.init()` names a method, so the method is a node"
        );
        assert!(!is_init_func(&def(DefKind::Function, "initialise", &[])));
    }

    #[test]
    fn an_init_body_has_no_def_fqn() {
        // The node rule lives in the language, not in the driver: `def_fqn`
        // answering `None` is what stops `init` from becoming a node, and
        // what sources the calls inside it at the package.
        let r = GoResolver;
        let cfg = module();
        let h = header("boot/a.go", "boot", vec![]);
        let probe: HashSet<NodeId> = HashSet::new();

        let init = def(DefKind::Function, "init", &[]);
        assert_eq!(r.def_fqn(&cfg, &h, &init.owner, &init, &probe), None);

        let method = def(DefKind::Method, "init", &["Conn"]);
        assert_eq!(
            r.def_fqn(&cfg, &h, &method.owner, &method, &probe),
            Some(Fqn::new("example.com/app/boot#Conn.init"))
        );

        // The edge source for a call inside `init` is the same `None`.
        let encloser = Encloser {
            path: vec!["init".into()],
            kind: DefKind::Function,
        };
        let synthetic = encloser.as_definition().expect("nameable");
        assert_eq!(
            r.def_fqn(&cfg, &h, &synthetic.owner, &synthetic, &probe),
            None
        );
    }

    #[test]
    fn an_encloser_and_its_definition_build_the_same_fqn() {
        // The edge source and the definition node must be byte-identical, or
        // an edge points at a node that does not exist.
        let r = GoResolver;
        let cfg = module();
        let h = header("server/server.go", "server", vec![]);
        let probe: HashSet<NodeId> = HashSet::new();

        let method = def(DefKind::Method, "Handle", &["Handler"]);
        let from_def = r.def_fqn(&cfg, &h, &method.owner, &method, &probe);
        assert_eq!(
            from_def,
            Some(Fqn::new("example.com/app/server#Handler.Handle"))
        );

        let encloser = Encloser {
            path: vec!["Handler".into(), "Handle".into()],
            kind: DefKind::Method,
        };
        let synthetic = encloser.as_definition().expect("nameable");
        assert_eq!(
            r.def_fqn(&cfg, &h, &synthetic.owner, &synthetic, &probe),
            from_def
        );
    }

    #[test]
    fn a_files_container_is_its_package_path() {
        let r = GoResolver;
        let cfg = module();
        let probe: HashSet<NodeId> = HashSet::new();
        let module_def = def(DefKind::Module, "server", &[]);

        let nested = header("server/server.go", "server", vec![]);
        assert_eq!(
            r.def_fqn(&cfg, &nested, &[], &module_def, &probe),
            Some(Fqn::new("example.com/app/server"))
        );
        // A file at the module root is the module's own package.
        let root = header("main.go", "main", vec![]);
        let main_def = def(DefKind::Module, "main", &[]);
        assert_eq!(
            r.def_fqn(&cfg, &root, &[], &main_def, &probe),
            Some(Fqn::new("example.com/app"))
        );
    }

    #[test]
    fn a_nested_module_is_not_this_scans_file() {
        let mut cfg = module();
        cfg.nested_modules = vec!["tools".into()];
        let r = GoResolver;
        assert!(r.owns_file(&cfg, "server/server.go"));
        assert!(!r.owns_file(&cfg, "tools/gen/main.go"));
        // A prefix match is not a directory match.
        assert!(r.owns_file(&cfg, "toolsx/main.go"));
        assert!(r.owns_file(&cfg, "tools.go"));
    }

    #[test]
    fn an_external_test_package_is_a_package_of_its_own() {
        // `package graph_test` in the directory of package `graph`.
        assert_eq!(
            package_path_for_file(
                "graph/graph_ext_test.go",
                "example.com/app/graph",
                Some("graph_test"),
                "graph"
            ),
            "example.com/app/graph!test"
        );
        // An in-package test file is the production package.
        assert_eq!(
            package_path_for_file(
                "graph/graph_test.go",
                "example.com/app/graph",
                Some("graph"),
                "graph"
            ),
            "example.com/app/graph"
        );
        // A directory whose package genuinely is called `foo_test` is not a
        // test package: the name it declares is the name it uses.
        assert_eq!(
            package_path_for_file(
                "foo_test/x_test.go",
                "example.com/app/foo_test",
                Some("foo_test"),
                "foo_test"
            ),
            "example.com/app/foo_test"
        );
        // No package clause parsed: the directory's package path stands.
        assert_eq!(
            package_path_for_file("graph/graph.go", "example.com/app/graph", None, "graph"),
            "example.com/app/graph"
        );
    }

    #[test]
    fn an_external_test_package_needs_a_test_file() {
        // `package foo_test` in a non-`_test.go` file is not a Go external
        // test package — the toolchain rejects that directory outright — so
        // the suffix alone was never the rule.
        assert!(!is_external_test_package("x/foo.go", "foo_test", "foo"));
        assert!(is_external_test_package("x/foo_test.go", "foo_test", "foo"));
        assert!(
            !is_external_test_package("x/foo_test.go", "foo", "foo"),
            "an in-package test file stays in the production namespace",
        );
    }

    #[test]
    fn the_test_package_identity_cannot_collide_with_a_directory() {
        // `#` is forbidden in a Go module path, so `{dir}#test` is an
        // identity no real directory can claim. With a `_test` suffix the
        // external test package of `graph` and a sibling directory named
        // `graph_test` shared one namespace.
        let external = package_path_for_file(
            "graph/graph_ext_test.go",
            "example.com/app/graph",
            Some("graph_test"),
            "graph",
        );
        let sibling = package_path_for_file(
            "graph_test/x.go",
            "example.com/app/graph_test",
            Some("graph_test"),
            "graph_test",
        );
        assert_eq!(external, "example.com/app/graph!test");
        assert_eq!(sibling, "example.com/app/graph_test");
        assert_ne!(external, sibling);
    }

    #[test]
    fn internal_import_resolves_against_the_probe() {
        let r = GoResolver;
        let cfg = module();
        let pkg = node_id(Domain::Go, "example.com/app/util");
        let mut table = HashSet::new();
        assert_eq!(
            r.resolve_import(&cfg, "example.com/app/util", &table)
                .outcome,
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
        );
        table.insert(pkg);
        assert_eq!(
            r.resolve_import(&cfg, "example.com/app/util", &table)
                .outcome,
            Outcome::Resolved(pkg)
        );
    }

    #[test]
    fn stdlib_known_dep_and_unknown_imports_classify() {
        let r = GoResolver;
        let cfg = module();
        let t: HashSet<NodeId> = HashSet::new();
        assert_eq!(
            r.resolve_import(&cfg, "net/http", &t).outcome,
            Outcome::External("std:net/http".into())
        );
        assert_eq!(
            r.resolve_import(&cfg, "github.com/pkg/errors", &t).outcome,
            Outcome::External("github.com/pkg/errors".into())
        );
        assert_eq!(
            r.resolve_import(&cfg, "github.com/nobody/mystery", &t)
                .outcome,
            Outcome::Unresolved(UnresolvedReason::UnknownPackage)
        );
    }

    #[test]
    fn plain_call_probes_same_package() {
        let r = GoResolver;
        let cfg = module();
        let helper = node_id(Domain::Go, "example.com/app/server#helper");
        let mut table = HashSet::new();
        let miss = r.resolve_call(&cfg, &call(named(&["helper"])), &scope(), &table);
        assert_eq!(
            miss.outcome,
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
        );
        assert_eq!(miss.candidates, vec![helper]); // the miss is recorded
        table.insert(helper);
        let hit = r.resolve_call(&cfg, &call(named(&["helper"])), &scope(), &table);
        assert_eq!(hit.outcome, Outcome::Resolved(helper));
    }

    #[test]
    fn builtins_are_external() {
        let r = GoResolver;
        let cfg = module();
        let t: HashSet<NodeId> = HashSet::new();
        let res = r.resolve_call(&cfg, &call(named(&["len"])), &scope(), &t);
        assert_eq!(res.outcome, Outcome::External("go:builtin".into()));
    }

    #[test]
    fn unaliased_imports_bind_without_module_version_suffixes() {
        let h = header(
            "server/server.go",
            "server",
            vec![
                unaliased("gopkg.in/yaml.v3"),
                unaliased("github.com/foo/bar/v2"),
                unaliased("example.com/plain"),
            ],
        );
        let r = GoResolver;
        let cfg = module(); // nothing indexed: every binding is the heuristic
        let s = file_scope(&r, &cfg, r.package_path(&cfg, "server"), &h);
        assert_eq!(
            s.imports.get("yaml").map(String::as_str),
            Some("gopkg.in/yaml.v3")
        );
        assert_eq!(
            s.imports.get("bar").map(String::as_str),
            Some("github.com/foo/bar/v2")
        );
        assert_eq!(
            s.imports.get("plain").map(String::as_str),
            Some("example.com/plain")
        );
        assert_eq!(s.imports.len(), 3);

        // The symptom the binding name causes: an unknown qualifier is read
        // as a variable, so every `yaml.X()` call would need type inference.
        let t: HashSet<NodeId> = HashSet::new();
        let res = r.resolve_call(&cfg, &call(named(&["yaml", "Unmarshal"])), &s, &t);
        assert_eq!(res.outcome, Outcome::External("gopkg.in/yaml.v3".into()));
    }

    #[test]
    fn a_known_internal_package_binds_its_declared_name_not_its_directory() {
        let h = header(
            "server/server.go",
            "server",
            vec![
                unaliased("example.com/app/utilx"), // declares `package util`
                unaliased("example.com/app/other"), // not indexed yet
                unaliased("gopkg.in/yaml.v3"),      // external: never indexed
            ],
        );
        let mut cfg = module();
        cfg.package_names
            .insert("example.com/app/utilx".to_string(), "util".to_string());
        let r = GoResolver;
        let s = file_scope(&r, &cfg, r.package_path(&cfg, "server"), &h);

        assert_eq!(
            s.imports.get("util").map(String::as_str),
            Some("example.com/app/utilx"),
            "the declared name is the binding"
        );
        assert!(
            !s.imports.contains_key("utilx"),
            "the directory name is not a binding: {:?}",
            s.imports
        );
        // Nothing known about the package: the path is the only evidence.
        assert_eq!(
            s.imports.get("other").map(String::as_str),
            Some("example.com/app/other")
        );
        assert_eq!(
            s.imports.get("yaml").map(String::as_str),
            Some("gopkg.in/yaml.v3")
        );
    }

    #[test]
    fn candidates_record_exactly_what_was_probed() {
        // `candidates` feeds the invalidation index: a candidate listed but
        // never probed would wake this reference for an edit that could not
        // have changed its outcome.
        let r = GoResolver;
        let cfg = module();
        let same_pkg = node_id(Domain::Go, "example.com/app/server#helper");
        let dot = node_id(Domain::Go, "example.com/app/util#helper");
        let mut s = scope();
        s.dot_imports.push("example.com/app/util".into());

        let mut table = HashSet::new();
        let miss = r.resolve_call(&cfg, &call(named(&["helper"])), &s, &table);
        // A total miss probes every candidate, so it records every candidate.
        assert_eq!(miss.candidates, vec![same_pkg, dot]);

        table.insert(same_pkg);
        let hit = r.resolve_call(&cfg, &call(named(&["helper"])), &s, &table);
        assert_eq!(hit.outcome, Outcome::Resolved(same_pkg));
        assert_eq!(hit.candidates.len(), 1, "the dot-import was never probed");
        assert_eq!(hit.candidates, vec![same_pkg]);
    }

    #[test]
    fn a_package_level_definition_beats_the_builtin_of_the_same_name() {
        // Go's universe scope is the outermost one: a package-level `min`
        // shadows the builtin, so the builtin answer is only correct once
        // every candidate in scope has been probed and missed.
        let r = GoResolver;
        let cfg = module();
        let min = node_id(Domain::Go, "example.com/app/server#min");
        let mut table = HashSet::new();
        let builtin = r.resolve_call(&cfg, &call(named(&["min"])), &scope(), &table);
        assert_eq!(builtin.outcome, Outcome::External("go:builtin".into()));
        assert_eq!(builtin.candidates, vec![min]); // the miss is still recorded
        table.insert(min);
        let shadowed = r.resolve_call(&cfg, &call(named(&["min"])), &scope(), &table);
        assert_eq!(shadowed.outcome, Outcome::Resolved(min));
        assert_eq!(shadowed.candidates, vec![min]);
    }

    #[test]
    fn qualified_calls_classify_by_import_table() {
        let r = GoResolver;
        let cfg = module();
        let target = node_id(Domain::Go, "example.com/app/util#Parse");
        let mut table = HashSet::new();
        table.insert(target);
        let internal = r.resolve_call(&cfg, &call(named(&["util", "Parse"])), &scope(), &table);
        assert_eq!(internal.outcome, Outcome::Resolved(target));
        let external = r.resolve_call(&cfg, &call(named(&["errors", "Wrap"])), &scope(), &table);
        assert_eq!(
            external.outcome,
            Outcome::External("github.com/pkg/errors".into())
        );
        let variable = r.resolve_call(&cfg, &call(named(&["conn", "Close"])), &scope(), &table);
        assert_eq!(
            variable.outcome,
            Outcome::Unresolved(UnresolvedReason::NeedsTypeInference)
        );
    }

    #[test]
    fn only_a_two_segment_name_reaches_the_import_table() {
        // The arity dispatch is the whole of it: one segment is the package
        // block, two is `QualifiedIdent`, and everything else needs a type.
        // A three-segment chain falling into the two-segment arm would
        // silently reclassify the largest unresolved bucket there is.
        let r = GoResolver;
        let cfg = module();
        let t: HashSet<NodeId> = HashSet::new();
        let inference = Outcome::Unresolved(UnresolvedReason::NeedsTypeInference);

        let chain = r.resolve_call(
            &cfg,
            &call(named(&["util", "Parse", "Inner"])),
            &scope(),
            &t,
        );
        assert_eq!(chain.outcome, inference);
        assert!(chain.candidates.is_empty());

        let empty = r.resolve_call(&cfg, &call(named(&[])), &scope(), &t);
        assert_eq!(empty.outcome, inference);
        assert!(empty.candidates.is_empty());

        let expr = r.resolve_call(
            &cfg,
            &call(RefTarget {
                root: TargetRoot::Expr,
                segments: vec!["apply".into()],
            }),
            &scope(),
            &t,
        );
        assert_eq!(expr.outcome, inference);
        assert!(expr.candidates.is_empty());

        // A `this` root with no enclosing method states a receiver type it
        // never wrote: `NeedsReceiverType`, never the import table and never
        // an invented edge.
        let this = r.resolve_call(
            &cfg,
            &call(RefTarget {
                root: TargetRoot::This { qualifier: vec![] },
                segments: vec!["m".into()],
            }),
            &scope(),
            &t,
        );
        assert_eq!(
            this.outcome,
            Outcome::Unresolved(UnresolvedReason::NeedsReceiverType),
        );
    }

    #[test]
    fn a_receiver_resolves_against_the_type_its_own_signature_states() {
        // Go's `this.m()`. The encloser carries `[ReceiverType, Method]` and
        // a method's FQN is `{pkg}#{Type}.{name}`, so this is a declared-type
        // lookup — the same one Java's X-02 and Python's E-01 perform for the
        // same shape, which is why all five tier-1 languages now count it in
        // both terms of the rate instead of one language alone excluding it.
        let r = GoResolver;
        let cfg = module();
        let s = scope();
        let helper = node_id(Domain::Go, "example.com/app/server#Conn.helper");
        let mut table = HashSet::new();
        table.insert(helper);

        let mut site = call(RefTarget {
            root: TargetRoot::This { qualifier: vec![] },
            segments: vec!["helper".into()],
        });
        site.enclosing = Some(Encloser {
            path: vec!["Conn".into(), "Run".into()],
            kind: DefKind::Method,
        });
        let hit = Resolver::resolve(&r, &cfg, &s, &site, &table);
        assert_eq!(hit.outcome, Outcome::Resolved(helper));
        assert_eq!(hit.candidates, vec![helper], "exactly what was probed");

        // A member the receiver type does not declare is not this
        // repository's bug: Go promotes an embedded type's members into the
        // outer method set, and this track indexes neither embedding nor
        // struct fields. `NoMatchingDefinition` would blame the corpus for
        // arthron's own missing work.
        let mut miss = site.clone();
        miss.target.segments = vec!["promoted".into()];
        assert_eq!(
            Resolver::resolve(&r, &cfg, &s, &miss, &table).outcome,
            Outcome::Unresolved(UnresolvedReason::NeedsReceiverType),
        );

        // `t.a.b()`: the receiver of `b` is the field `a`, and no file states
        // its type. That is inference, and saying `NeedsReceiverType` would
        // claim a declared type nobody wrote.
        let mut deep = site.clone();
        deep.target.segments = vec!["a".into(), "b".into()];
        assert_eq!(
            Resolver::resolve(&r, &cfg, &s, &deep, &table).outcome,
            Outcome::Unresolved(UnresolvedReason::NeedsTypeInference),
        );
    }

    #[test]
    fn a_locally_bound_reference_is_local_binding_with_no_candidates() {
        // The same site twice: the extractor's file-local verdict is the
        // only difference, and it is what stops a wrong edge being emitted.
        let r = GoResolver;
        let cfg = module();
        let helper = node_id(Domain::Go, "example.com/app/server#helper");
        let mut table = HashSet::new();
        table.insert(helper);
        let s = scope();

        let free = call(named(&["helper"]));
        assert_eq!(
            Resolver::resolve(&r, &cfg, &s, &free, &table).outcome,
            Outcome::Resolved(helper),
        );

        let bound = Reference {
            locally_bound: true,
            ..call(named(&["helper"]))
        };
        let res = Resolver::resolve(&r, &cfg, &s, &bound, &table);
        assert_eq!(
            res.outcome,
            Outcome::Unresolved(UnresolvedReason::LocalBinding)
        );
        assert!(
            res.candidates.is_empty(),
            "a local binding is decidable from one file, so no definition \
             edit anywhere can change it and nothing is probed",
        );
    }

    #[test]
    fn resolve_routes_imports_and_calls_to_their_arms() {
        let r = GoResolver;
        let cfg = module();
        let t: HashSet<NodeId> = HashSet::new();
        let s = scope();
        let import = Reference {
            kind: RefKind::Import,
            raw_target: "net/http".into(),
            ..call(named(&["net/http"]))
        };
        assert_eq!(
            Resolver::resolve(&r, &cfg, &s, &import, &t).outcome,
            Outcome::External("std:net/http".into())
        );
        assert_eq!(
            Resolver::resolve(&r, &cfg, &s, &call(named(&["len"])), &t).outcome,
            Outcome::External("go:builtin".into())
        );
    }
}
