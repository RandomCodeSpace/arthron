//! The Java resolver: all cross-file linking for Java. Never drops.
//!
//! Probes a [`SymbolProbe`] with candidate identities built by
//! [`crate::track_java::fqn`] and classifies every reference into exactly one
//! [`Outcome`]. Every candidate probed — hit or miss — is returned, because
//! the invalidation index is what wakes this reference when a definition it
//! missed on appears.
//!
//! Case identifiers in the comments (`N-03`, `X-02`, `B-05`, …) name numbered
//! cases in the Java case study, which is this track's contract.
//!
//! # What this resolver can and cannot see
//!
//! Two of the case study's deltas are not available in the core as built, and
//! both are worked around here rather than around them:
//!
//! * **No set-valued probe (M-04).** [`crate::lang::Resolver::index_keys`] is
//!   declared but never called by the driver, so the `OVERLOAD`/`VARARGS`
//!   multimap cannot be built. The arity key is therefore the definition's
//!   *identity* whenever it is unique, and an ambiguous set is represented by
//!   a [`DefKind::Alias`] node the extractor emits at the shared key. Unique
//!   callables keep that arity identity and expose their written parameter
//!   shape through a forwarding signature alias. Typed applicability probes
//!   signatures uniformly; an untyped probe that lands on the shared marker
//!   still reports `AmbiguousOverload`. See [`crate::track_java::fqn`].
//! * **The supertype closure is two facts, not one (H-01).** For a type the
//!   file being resolved declares, `extends`/`implements` are a single-file
//!   fact ([`TypeDecl`]) and the walk reads them straight off the scope. For
//!   every other type they come from the driver's supertype phase, which
//!   resolved that file's `Inherit` references before any member reference ran
//!   and left the relation in the store — [`JavaResolver::lookup`] walks it
//!   one hop at a time, so a member declared three files above a receiver is
//!   found.
//!
//!   What stays is genuinely unreachable rather than merely unbuilt: §4.3.2
//!   puts `java.lang.Object` above every class and no scan of a repository
//!   indexes it, so a member found nowhere in the closure is still
//!   [`UnresolvedReason::UnindexedSupertype`] — B-05's own reason, and an
//!   honest floor rather than a rate to be gamed down.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::lang::{
    Entry, FileFacts, FileIndex, Language, LayoutError, RefKeyRefinement, Resolution, Resolver,
    Supertypes, SymbolProbe,
};
use crate::model::{
    DefFacets, DefKind, Definition, Fqn, NodeId, RefKind, Reference, TargetRoot, node_id,
};
use crate::track_java::JavaLang;
use crate::track_java::extract::{Binding, BindingKind, ErasedType, JavaHeader, TypeDecl};
use crate::track_java::fqn;
use crate::{Outcome, UnresolvedReason};

/// Public types of `java.lang`, which JLS §7.3 imports into every compilation
/// unit as if by `import java.lang.*;`.
///
/// A *list*, not a prefix rule, and deliberately not exhaustive: it is the
/// tier-4 fallback that keeps `String`, `Override` and `IllegalStateException`
/// from being reported as `NoMatchingDefinition` in a corpus that compiles.
/// B-03's Δ is to ship the JDK's exported-package set per release line; until
/// that exists this is the documented approximation, and it can only produce
/// `External` — never a resolved edge — so being short is a lowered rate and
/// never a wrong link. Sorted, for binary search.
const JAVA_LANG: &[&str] = &[
    "Appendable",
    "ArithmeticException",
    "ArrayIndexOutOfBoundsException",
    "ArrayStoreException",
    "AssertionError",
    "AutoCloseable",
    "Boolean",
    "Byte",
    "CharSequence",
    "Character",
    "Class",
    "ClassCastException",
    "ClassLoader",
    "ClassNotFoundException",
    "CloneNotSupportedException",
    "Cloneable",
    "Comparable",
    "Deprecated",
    "Double",
    "Enum",
    "Error",
    "Exception",
    "ExceptionInInitializerError",
    "Float",
    "FunctionalInterface",
    "IllegalAccessException",
    "IllegalArgumentException",
    "IllegalStateException",
    "IndexOutOfBoundsException",
    "InstantiationException",
    "Integer",
    "InterruptedException",
    "Iterable",
    "LinkageError",
    "Long",
    "Math",
    "NegativeArraySizeException",
    "NoClassDefFoundError",
    "NoSuchFieldException",
    "NoSuchMethodException",
    "NullPointerException",
    "Number",
    "NumberFormatException",
    "Object",
    "OutOfMemoryError",
    "Override",
    "Package",
    "Process",
    "ProcessBuilder",
    "Readable",
    "Record",
    "ReflectiveOperationException",
    "Runnable",
    "Runtime",
    "RuntimeException",
    "SafeVarargs",
    "SecurityException",
    "Short",
    "StackOverflowError",
    "StrictMath",
    "String",
    "StringBuffer",
    "StringBuilder",
    "StringIndexOutOfBoundsException",
    "SuppressWarnings",
    "System",
    "Thread",
    "ThreadGroup",
    "ThreadLocal",
    "Throwable",
    "UnsupportedOperationException",
    "Void",
];

/// The external payload every `java.lang` reference reaches.
const JAVA_LANG_PACKAGE: &str = "jdk:java.lang";

/// Whether a simple name is a public type of `java.lang` (§7.3).
fn is_java_lang(name: &str) -> bool {
    JAVA_LANG.binary_search(&name).is_ok()
}

/// Whether a call names a member every class inherits from `java.lang.Object`
/// (§4.3.2), at an arity `Object` actually declares.
///
/// Checked *after* every candidate has missed, for the same reason Go probes
/// its builtins last: a type that declares its own `toString()` must win.
fn is_object_member(name: &str, argc: Option<u32>) -> bool {
    matches!(
        (name, argc),
        ("equals", Some(1))
            | ("wait", Some(0..=2))
            | (
                "hashCode"
                    | "toString"
                    | "getClass"
                    | "notify"
                    | "notifyAll"
                    | "clone"
                    | "finalize",
                Some(0),
            )
    )
}

/// Whether a package belongs to the JDK, by JEP 261's module naming.
///
/// B-03: a `java.*` prefix test is safe; a `javax.*` one is **not** — JEP 320
/// removed `javax.xml.bind`, `javax.activation`, `javax.annotation` and CORBA
/// from the JDK in Java 11, so those are third-party artifacts on any modern
/// classpath. `javax` is therefore deliberately absent from this list.
fn is_jdk_package(package: &str) -> bool {
    let root = package.split('.').next().unwrap_or(package);
    matches!(root, "java" | "jdk" | "sun") || package.starts_with("com.sun.")
}

/// The `External` payload for a package outside the repository.
fn outside(package: &str) -> String {
    if is_jdk_package(package) {
        format!("jdk:{package}")
    } else {
        package.to_string()
    }
}

/// The symbol table, plus the log of every identity this resolution asked it
/// about.
///
/// One value rather than two parameters, because the two are one obligation:
/// [`Resolution::candidates`] must list exactly what was probed and nothing
/// else, or the invalidation index it feeds would wake this reference for
/// edits that could not change its outcome. Routing every lookup through
/// [`Probes::get`] makes that structural rather than remembered.
struct Probes<'a> {
    table: &'a dyn SymbolProbe,
    seen: Vec<NodeId>,
}

impl Probes<'_> {
    /// Ask the table about one identity, recording it hit or miss.
    fn get(&mut self, fqn: &str) -> Option<Entry> {
        let id = node_id(JavaLang::DOMAIN, fqn);
        self.seen.push(id);
        self.table.probe(&id)
    }

    /// Ask the table what a type sits under, recording the *type's* identity.
    ///
    /// Recorded through the same log as any other lookup, and that is the
    /// whole reason this method exists rather than a direct call: what a type
    /// extends is part of what its identity means, so a reference that read it
    /// has to be woken when it changes — exactly as one that read a
    /// definition's kind is.
    fn supers(&mut self, fqn: &str) -> Option<Supertypes> {
        let id = node_id(JavaLang::DOMAIN, fqn);
        self.seen.push(id);
        self.table.supertypes(&id)
    }
}

/// Java's project layout.
///
/// B-04: `deps` is deliberately absent rather than optional-and-unused. Maven
/// POMs are statically parseable and Gradle build scripts are programs that
/// are not, so a Java project may have no `go.mod` equivalent at all and the
/// design must not require one. `External` is therefore decided by *absence
/// from the indexed definition set plus package attribution* (B-01, B-02) and
/// never by the shape of a name. What is left is what the scan itself learns:
/// which packages and modules this repository declares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JavaConfig {
    /// Every package a scanned file declares (P-01, P-02). Many source roots
    /// fold into one namespace here, because a package is what a compilation
    /// unit *declares* and never what directory it sits in.
    pub packages: BTreeSet<String>,
    /// Every module a `module-info.java` declares (P-05).
    pub modules: BTreeSet<String>,
}

/// The Java resolver's per-file scope: the case study's §9 chain, as far as
/// one file plus the symbol table can state it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JavaScope {
    /// The compilation unit's container FQN (P-01).
    container: String,
    /// I-01: simple name → the canonical name a single-type import binds it
    /// to, split on `.`.
    single_type: HashMap<String, Vec<String>>,
    /// I-04: `(member simple name, owner canonical name)`. One declaration
    /// binds a *member name* — every overload, possibly a field, possibly a
    /// member type — which is why this is not a name→FQN map.
    single_static: Vec<(String, Vec<String>)>,
    /// I-02: packages (or types) whose members are imported on demand.
    type_on_demand: Vec<Vec<String>>,
    /// I-05: owner types whose static members are imported on demand.
    static_on_demand: Vec<Vec<String>>,
    /// I-07: modules imported on demand (JEP 511).
    module_imports: Vec<Vec<String>>,
    /// Types this file declares: simple name → every nesting path with that
    /// simple name.
    file_types: HashMap<String, Vec<Vec<String>>>,
    /// Nesting path joined by `$` → what that type declares as its
    /// supertypes (H-01).
    supers: HashMap<String, TypeDecl>,
    /// X-02's declared-type environment, with the extents §6.3 scopes it by.
    bindings: Vec<Binding>,
    /// T-03..T-05's type frames that are not nodes, with the extents that say
    /// which sites are inside them.
    erased: Vec<ErasedType>,
}

impl JavaScope {
    /// The innermost declaration of `name` visible at `site`, in the value
    /// namespace (§6.5.1 keeps the tables apart, and §6.4.1 makes the
    /// innermost declaration win).
    fn binding_at(&self, name: &str, site: u32) -> Option<&Binding> {
        self.bindings
            .iter()
            .filter(|b| {
                b.name == name
                    && b.start <= site
                    && site < b.end
                    && matches!(
                        b.kind,
                        BindingKind::Field
                            | BindingKind::Local
                            | BindingKind::Parameter
                            | BindingKind::PatternVariable
                            | BindingKind::CatchParameter
                    )
            })
            // Innermost wins: the latest region to open that still contains
            // the site is the one §6.4.1 shadows the others with.
            .max_by_key(|b| b.start)
    }

    /// The type parameter `name` names at `site`, if one is in scope (§4.4).
    ///
    /// A separate lookup from [`JavaScope::binding_at`] because §6.5.1 keeps
    /// the two tables apart: a local called `T` does not make the type name
    /// `T` a variable, and the reverse.
    fn type_parameter_at(&self, name: &str, site: u32) -> Option<&Binding> {
        self.bindings
            .iter()
            .filter(|b| {
                b.name == name
                    && b.start <= site
                    && site < b.end
                    && b.kind == BindingKind::TypeParameter
            })
            .max_by_key(|b| b.start)
    }

    /// The field `name` declared and visible at `site`.
    ///
    /// Narrower than [`JavaScope::binding_at`] on purpose: `this.f` names a
    /// *field* (§15.8.3), and a local of the same name is a different
    /// declaration that `this` cannot reach.
    fn field_at(&self, name: &str, site: u32) -> Option<&Binding> {
        self.bindings
            .iter()
            .filter(|b| {
                b.name == name && b.start <= site && site < b.end && b.kind == BindingKind::Field
            })
            .max_by_key(|b| b.start)
    }

    /// The anonymous class body a creation site spanning `start..end` writes,
    /// if it writes one (T-04).
    ///
    /// An `object_creation_expression` ends exactly where its class body
    /// does, which is what picks out the frame belonging to *this* site
    /// rather than one nested inside its arguments.
    fn anonymous_body_at(&self, start: u32, end: u32) -> Option<&ErasedType> {
        self.erased.iter().find(|f| f.end == end && f.start > start)
    }

    /// Every erased type frame containing `site`, innermost first.
    ///
    /// Innermost first because that is §15.12.1's own order: the search runs
    /// from the innermost enclosing type declaration outward, and a frame
    /// nested inside another opens later.
    fn erased_at(&self, site: u32) -> Vec<&ErasedType> {
        let mut frames: Vec<&ErasedType> = self
            .erased
            .iter()
            .filter(|f| f.start <= site && site < f.end)
            .collect();
        frames.sort_by_key(|f| std::cmp::Reverse(f.start));
        frames
    }
}

/// A type a reference named, once it has been placed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    /// A type with a definition in this repository.
    InRepo {
        /// Its identity string.
        fqn: String,
        /// Its nesting path, when it is declared in the file being resolved —
        /// which is the only case whose supertypes are readable (H-01).
        local: Option<Vec<String>>,
    },
    /// A type outside the repository, attributed to the qualifier that named
    /// it (B-02).
    Outside(String),
    /// The type could not be placed, with the reason it could not.
    Failed(UnresolvedReason),
}

/// What a member lookup found.
enum Member {
    /// Exactly one declaration of that name and arity.
    Found(NodeId),
    /// The owner declares two or more; §15.12.2.5 needs the argument types
    /// and this resolver does not compute them (X-04).
    Ambiguous,
    /// No declaration, plus whether some supertype was out of reach (B-05).
    Missing { unindexed: bool },
}

/// Where a reference sits.
///
/// The two facts every name resolution needs about a site and neither of
/// which is in the name: which nameable types lexically enclose it (§15.12.1
/// walks them outward) and its byte offset, which is what decides which
/// bindings (§6.3) and which erased type frames (T-03..T-05) contain it.
#[derive(Debug, Clone, Copy)]
struct Site<'a> {
    /// The enclosing nameable type chain, outermost first.
    types: &'a [String],
    /// Byte offset of the reference.
    at: u32,
}

/// The overload-discriminating facts one invocation site states.
#[derive(Debug, Clone, Copy)]
struct Invocation<'a> {
    /// Written argument count.
    argc: Option<u32>,
    /// Written argument types, when every one is file-local.
    arguments: Option<&'a [String]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvocationPhase {
    Strict,
    Loose,
    Varargs,
}

#[derive(Debug, Clone)]
struct Applicable {
    owner: String,
    target: NodeId,
    depths: Vec<u8>,
}

#[derive(Debug, Clone)]
struct TypedSite<'a> {
    owner: &'a str,
    local: Option<Vec<String>>,
    name: &'a str,
    arguments: &'a [String],
}

#[derive(Debug, Default)]
struct Applicability {
    candidates: Vec<Applicable>,
    saw_member: bool,
    unindexed: bool,
    ambiguous: bool,
}

/// Every declaration one *member name* reaches on a type (I-04, C-08).
struct NameGroup {
    /// Distinct declarations found, most specific key first.
    found: Vec<NodeId>,
    /// An overload-set marker was hit: two or more declarations share a key
    /// and neither took it (M-01).
    ambiguous: bool,
    /// Some supertype was external or unindexed, so the set may be short.
    unindexed: bool,
}

/// What a member lookup inside an erased type frame found (T-03..T-05).
enum FrameMember {
    /// The frame declares it itself. Real, and by design not a node — so
    /// there is no honest edge, and in particular the search must not walk
    /// outward and find a same-named member of a type the site is not in.
    Own,
    /// Found on a supertype the frame names, which *is* nameable.
    Found(NodeId),
    /// The supertype declares two or more at this arity.
    Ambiguous,
    /// Not found, plus whether the frame's supertypes were out of reach.
    Missing {
        /// Some supertype of the frame is external or unindexed.
        unindexed: bool,
    },
}

/// All Java linking decisions. Stateless: every fact rides on the
/// [`JavaConfig`] the driver hands it.
#[derive(Debug, Clone, Copy, Default)]
pub struct JavaResolver;

impl JavaResolver {
    /// Whether a repo-relative path is build output rather than source.
    ///
    /// P-07 wants `target/`, `build/`, `out/` and `bin/` skipped; G-01 wants
    /// `target/generated-sources/**` and `build/generated/sources/**` *read*,
    /// because after any build they are real `.java` on disk declaring
    /// members hand-written source names. [`Language::skip_dirs`] is a flat
    /// list of names and cannot state that exception, so the rule lives here,
    /// where the whole path is visible.
    fn is_build_output(rel_path: &str) -> bool {
        let generated = rel_path.contains("target/generated-sources/")
            || rel_path.contains("build/generated/sources/");
        if generated {
            return false;
        }
        rel_path
            .split('/')
            .any(|segment| matches!(segment, "target" | "build" | "out" | "bin"))
    }

    /// The type `path` names in this compilation unit, if the store has it.
    fn probe_type(
        &self,
        scope: &JavaScope,
        package: &str,
        path: &[String],
        local: Option<Vec<String>>,
        p: &mut Probes<'_>,
    ) -> Option<Owner> {
        let _ = scope;
        let fqn = fqn::type_fqn(package, path);
        match p.get(&fqn) {
            Some(Entry::Definition {
                kind: DefKind::Type,
                ..
            }) => Some(Owner::InRepo { fqn, local }),
            _ => None,
        }
    }

    /// Resolve a *simple* type name through JLS §6.5.5.1's tiers (N-03).
    ///
    /// Ordered, probed one at a time, stopping at the first hit — and every
    /// probe recorded, because a name that resolved at tier 4 must be woken
    /// when a tier-3 declaration appears.
    fn simple_type(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        name: &str,
        enclosing: &[String],
        p: &mut Probes<'_>,
    ) -> Option<Owner> {
        // Tier 1: member types of the enclosing types, and top-level types of
        // this compilation unit (§6.5.5.1, §7.3). Both are declarations this
        // file makes, so they are one lookup.
        if let Some(paths) = scope.file_types.get(name) {
            let chosen = paths
                .iter()
                // A member type of a type enclosing the site shadows a
                // same-named member type of an unrelated one.
                .find(|path| path.len() >= 2 && enclosing.starts_with(&path[..path.len() - 1]))
                .or_else(|| (paths.len() == 1).then(|| &paths[0]));
            if let Some(path) = chosen {
                let path = path.clone();
                if let Some(owner) =
                    self.probe_type(scope, &scope.container, &path, Some(path.clone()), p)
                {
                    return Some(owner);
                }
            } else {
                // Two member types of unrelated enclosing types share the
                // simple name and nothing here decides between them.
                return Some(Owner::Failed(UnresolvedReason::AmbiguousName));
            }
        }
        // Still tier 1: §8.5 makes a supertype's member type a member of the
        // subtype, so `State` inside `class Sub extends Base` names
        // `Base$State` with nothing written in this file but `extends`.
        if let Some(owner) = self.inherited_type(cfg, scope, name, enclosing, p) {
            return Some(owner);
        }
        // Tier 2: single-type imports (§7.5.1), which shadow same-package and
        // on-demand declarations alike.
        if let Some(canonical) = scope.single_type.get(name) {
            return Some(self.canonical_type(cfg, scope, &canonical.clone(), p));
        }
        // Tier 3: other compilation units of the same package (§6.3, §7.3).
        if let Some(owner) = self.probe_type(
            scope,
            &scope.container,
            std::slice::from_ref(&name.to_string()),
            None,
            p,
        ) {
            return Some(owner);
        }
        // Tier 4: type-import-on-demand and the implicit `java.lang.*`, which
        // §7.3 places at exactly this tier and not above it.
        for package in &scope.type_on_demand {
            let package = package.join(".");
            if let Some(owner) = self.probe_type(
                scope,
                &package,
                std::slice::from_ref(&name.to_string()),
                None,
                p,
            ) {
                return Some(owner);
            }
        }
        if is_java_lang(name) {
            return Some(Owner::Outside(JAVA_LANG_PACKAGE.to_string()));
        }
        None
    }

    /// A member type `name` inherited by one of the types enclosing the site
    /// (§8.5, N-03 tier 1).
    ///
    /// The same closure [`JavaResolver::lookup`] walks for a member, walked
    /// for a *nested type* instead — and reachable for the same reason and no
    /// further: only a type this file declares states its own supertypes
    /// (H-01), so the walk starts at an enclosing type and stops wherever the
    /// names run out. Missing it sent every inherited `State` and `Builder`
    /// to `NoMatchingDefinition`, which is the one bucket reserved for our
    /// own bug.
    fn inherited_type(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        name: &str,
        enclosing: &[String],
        p: &mut Probes<'_>,
    ) -> Option<Owner> {
        let mut seen: HashSet<String> = HashSet::new();
        // Innermost first: a member type of the type the site sits in
        // shadows a same-named one on a type further out (§6.4.1).
        for depth in (1..=enclosing.len()).rev() {
            let path = &enclosing[..depth];
            // The enclosing type's *own* member types are `file_types`, which
            // the caller has already asked — a type declared in this file is
            // in it whatever its nesting. So the walk starts at the
            // supertypes, and a type that names none costs no probe at all.
            let Some(decl) = scope.supers.get(&path.join(&fqn::NEST.to_string())) else {
                continue;
            };
            let mut queue: Vec<(String, Option<Vec<String>>)> = Vec::new();
            for segments in decl.superclass.iter().chain(decl.interfaces.iter()) {
                if let Owner::InRepo { fqn, local } = self.canonical_type(cfg, scope, segments, p) {
                    queue.push((fqn, local));
                }
            }
            while let Some((type_fqn, local)) = queue.pop() {
                if !seen.insert(type_fqn.clone()) {
                    continue;
                }
                let nested = format!("{type_fqn}{}{name}", fqn::NEST);
                if let Some(Entry::Definition {
                    kind: DefKind::Type,
                    ..
                }) = p.get(&nested)
                {
                    return Some(Owner::InRepo {
                        fqn: nested,
                        local: local.map(|mut path| {
                            path.push(name.to_string());
                            path
                        }),
                    });
                }
                let Some(decl) = local
                    .as_ref()
                    .and_then(|path| scope.supers.get(&path.join(&fqn::NEST.to_string())))
                else {
                    continue;
                };
                let supers = decl.superclass.iter().chain(decl.interfaces.iter());
                for segments in supers {
                    if let Owner::InRepo { fqn, local } =
                        self.canonical_type(cfg, scope, segments, p)
                    {
                        queue.push((fqn, local));
                    }
                }
            }
        }
        None
    }

    /// Why a simple type name that missed every tier could not be placed.
    fn simple_type_miss(&self, cfg: &JavaConfig, scope: &JavaScope) -> UnresolvedReason {
        // I-02, I-05: a wildcard naming a package this scan never indexed can
        // supply any simple name, so we cannot even say what the name was.
        // Folding this into `NoMatchingDefinition` would destroy that
        // reason's meaning, which in a corpus that compiles is *our* bug.
        let unindexed_wildcard = scope
            .type_on_demand
            .iter()
            .chain(scope.static_on_demand.iter())
            .any(|segments| !cfg.packages.contains(&segments.join(".")))
            || !scope.module_imports.is_empty();
        if unindexed_wildcard {
            UnresolvedReason::WildcardImport
        } else {
            UnresolvedReason::NoMatchingDefinition
        }
    }

    /// Resolve a canonical (multi-segment) type name — an import's name, or a
    /// fully qualified reference.
    ///
    /// N-04 / §6.5.2: the package/type split is decided by longest-prefix
    /// match against the symbol table and by nothing else. When the table has
    /// no opinion the reference is still in a type-naming context, so §6.5.5
    /// makes its last segment a simple type name and its qualifier the thing
    /// that names it — which is exactly the `External` payload B-02 asks for.
    fn canonical_type(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        segments: &[String],
        p: &mut Probes<'_>,
    ) -> Owner {
        if segments.is_empty() {
            return Owner::Failed(UnresolvedReason::NoMatchingDefinition);
        }
        if segments.len() == 1 {
            return self
                .simple_type(cfg, scope, &segments[0], &[], p)
                .unwrap_or_else(|| Owner::Failed(self.simple_type_miss(cfg, scope)));
        }
        // Longest package prefix first: `a.b.C.D` under package `a.b` is the
        // nested type `C$D`, and under package `a.b.C` it is the type `D`.
        for split in (1..segments.len()).rev() {
            let package = segments[..split].join(".");
            if !cfg.packages.contains(&package) {
                continue;
            }
            if let Some(owner) = self.probe_type(scope, &package, &segments[split..], None, p) {
                return owner;
            }
        }
        // The head may be a type in scope, with the rest nesting inside it —
        // `Map.Entry` after `import java.util.Map;`, or `Outer.Inner`.
        if let Some(head) = self.simple_type(cfg, scope, &segments[0], &[], p) {
            return self.nest(head, &segments[1..], p);
        }
        self.attribute(cfg, segments)
    }

    /// The owner a multi-segment name no probe placed is attributed to
    /// (B-02, N-04).
    ///
    /// §6.5.5 makes the last segment a simple type name and its qualifier the
    /// thing that names it, so the qualifier is the package — which is
    /// exactly `External`'s payload. Unless the repository *declares* that
    /// package: then the table's opinion about it is complete and the type is
    /// absent from a package we indexed. Calling that `External` would let a
    /// definition that should exist here leave both terms of the resolution
    /// rate rather than being counted as the miss it is.
    fn attribute(&self, cfg: &JavaConfig, segments: &[String]) -> Owner {
        let package = segments[..segments.len() - 1].join(".");
        if cfg.packages.contains(&package) {
            return Owner::Failed(UnresolvedReason::NoMatchingDefinition);
        }
        Owner::Outside(outside(&package))
    }

    /// Extend a placed type through nested-type selections.
    fn nest(&self, head: Owner, rest: &[String], p: &mut Probes<'_>) -> Owner {
        let mut owner = head;
        for segment in rest {
            owner = match owner {
                Owner::InRepo { fqn, local } => {
                    let nested = format!("{fqn}{}{segment}", fqn::NEST);
                    match p.get(&nested) {
                        Some(Entry::Definition {
                            kind: DefKind::Type,
                            ..
                        }) => Owner::InRepo {
                            fqn: nested,
                            local: local.map(|mut path| {
                                path.push(segment.clone());
                                path
                            }),
                        },
                        // The selection is a member, not a nested type, and
                        // its type lives in a file this resolver cannot read.
                        _ => return Owner::Failed(UnresolvedReason::NeedsTypeInference),
                    }
                }
                // Everything selected out of an external type is external.
                other => return other,
            };
        }
        owner
    }

    /// Place the qualifier of a value-space chain, leftmost-first (§6.5.2):
    /// is the head a variable, then a type, then a package prefix?
    ///
    /// Returns the owner and how many segments it consumed.
    fn qualifier(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        segments: &[String],
        site: Site<'_>,
        p: &mut Probes<'_>,
    ) -> (Owner, usize) {
        if segments.is_empty() {
            return (Owner::Failed(UnresolvedReason::NoMatchingDefinition), 0);
        }
        // §6.4.2: a variable obscures a type, which obscures a package. X-02:
        // the declared type is written in this file, so this is a lookup and
        // not inference.
        if let Some(binding) = scope.binding_at(&segments[0], site.at) {
            let Some(declared) = binding.declared_type.clone() else {
                // A lambda parameter, an unreadable `var` (X-03), an array or
                // a primitive: the receiver is a name with no stated type.
                return (Owner::Failed(UnresolvedReason::NeedsTypeInference), 1);
            };
            let owner = self.declared_owner(cfg, scope, &declared, site.at, 0, p);
            return (owner, 1);
        }
        if let Some(head) = self.simple_type(cfg, scope, &segments[0], site.types, p) {
            let mut consumed = 1;
            let mut owner = head;
            // Greedy nesting: `Outer.Inner.staticCall` selects a type twice.
            while consumed < segments.len() {
                let next = self.nest(owner.clone(), std::slice::from_ref(&segments[consumed]), p);
                match next {
                    Owner::Failed(_) => break,
                    placed => {
                        owner = placed;
                        consumed += 1;
                    }
                }
            }
            return (owner, consumed);
        }
        for split in (1..=segments.len()).rev() {
            let package = segments[..split].join(".");
            if !cfg.packages.contains(&package) {
                continue;
            }
            if split == segments.len() {
                // The whole qualifier is a package: nothing to own a member.
                return (Owner::Failed(UnresolvedReason::NoMatchingDefinition), split);
            }
            if let Some(owner) = self.probe_type(
                scope,
                &package,
                std::slice::from_ref(&segments[split]),
                None,
                p,
            ) {
                return (self.nest(owner, &segments[split + 1..], p), segments.len());
            }
        }
        if segments.len() == 1 {
            // One segment and it is neither a variable nor a type in scope:
            // the type name itself is what could not be placed.
            return (Owner::Failed(self.simple_type_miss(cfg, scope)), 1);
        }
        // §6.5.2's reclassification asked the symbol table and the table had
        // no opinion — which is an opinion: nothing in this qualifier is
        // ours. That is the same conclusion `canonical_type` draws three
        // lines from here for the identical name in type position, and the
        // two positions disagreeing meant `java.util.Objects.requireNonNull`
        // was `AmbiguousName` while `java.util.List` was `External`.
        (self.attribute(cfg, segments), segments.len())
    }

    /// How many type-variable bounds are followed before giving up.
    ///
    /// `<A extends B, B extends C>` is legal and finite, and a bound may
    /// name an earlier parameter — but nothing in one file guarantees the
    /// chain is acyclic once recovery has been at the tree, so the walk is
    /// bounded rather than trusting.
    const BOUND_DEPTH: u32 = 8;

    /// The owner a *declared type name* places to, with X-07's type-variable
    /// rule applied.
    ///
    /// §4.4: a receiver whose declared type is a type variable is looked up
    /// on the variable's bound, which is written in the same file. §4.6: an
    /// unbounded one erases to `Object`, whose members are external. Without
    /// this, `T value; value.tag();` is `NoMatchingDefinition` — the bucket
    /// that is supposed to mean *our* bug — for a target that is one written
    /// `extends` clause away.
    fn declared_owner(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        declared: &[String],
        site: u32,
        depth: u32,
        p: &mut Probes<'_>,
    ) -> Owner {
        if declared.len() == 1
            && let Some(parameter) = scope.type_parameter_at(&declared[0], site)
        {
            let Some(bound) = parameter.declared_type.clone() else {
                // §4.6: the erasure of an unbounded type variable is
                // `Object`, which is never a definition of this repository.
                return Owner::Outside(JAVA_LANG_PACKAGE.to_string());
            };
            if depth >= Self::BOUND_DEPTH {
                return Owner::Failed(UnresolvedReason::NeedsTypeInference);
            }
            return self.declared_owner(cfg, scope, &bound, site, depth + 1, p);
        }
        // Array members need array-member modeling. Do not pass an array
        // spelling to ordinary type placement: its unsupported member stays
        // `NeedsTypeInference` without changing the row key.
        if Self::has_array_suffix(declared) {
            return Owner::Failed(UnresolvedReason::NeedsTypeInference);
        }
        self.canonical_type(cfg, scope, declared, p)
    }

    /// Whether a declared type carries one or more Java array suffixes.
    /// This mirrors [`Self::exact_type_spellings`]'s suffix walk without
    /// changing its alias-expansion behavior.
    fn has_array_suffix(declared: &[String]) -> bool {
        let Some(mut base) = declared.last().map(String::as_str) else {
            return false;
        };
        let mut found = false;
        loop {
            if let Some(stripped) = base.strip_suffix("[]") {
                base = stripped;
                found = true;
            } else if let Some(stripped) = base.strip_suffix("...") {
                base = stripped;
                found = true;
            } else {
                return found;
            }
        }
    }

    /// The greatest arity a *member-name* probe walks to.
    ///
    /// A method reference (C-08) and a single-static import (I-04) both name
    /// a member name rather than one declaration: §15.13.1 chooses the
    /// overload from the target functional-interface type and §7.5.3 imports
    /// every member of that name at once, so neither site states an arity.
    /// M-04's overload-set index would answer "which declarations carry this
    /// name" in one probe; [`Resolver::index_keys`] is declared and never
    /// driven, so the set is walked by arity instead and the walk has to
    /// stop somewhere.
    ///
    /// Eight is not a language limit — §8.4.1 has none below the JVM's 255 —
    /// and it is not where the answers stop either: commons-lang measures
    /// identically at 2, 4, 6, 8 and 12, because a name imported statically
    /// or written `Type::name` is a name someone types at a call site, and
    /// those are short. Eight is the headroom over that measured plateau,
    /// kept because one corpus is one corpus. Past the bound the outcome is
    /// whatever the shorter set said — a missed resolution, never a wrong
    /// edge.
    const NAME_PROBE_ARITY: u32 = 8;

    /// The most types one member lookup walks before it gives up.
    ///
    /// The `seen` set already makes the walk terminate; this bounds what it
    /// *costs*, which a cycle guard does not: a hierarchy is as wide as the
    /// corpus makes it and every reference pays for the whole of it. A walk
    /// cut here is a short walk, so it reports exactly what an unreadable
    /// supertype reports — never a wrong edge, and never `NoMatchingDefinition`
    /// on a search that did not finish. Sixty-four is far above commons-lang's
    /// deepest closure and cheap to raise if a corpus ever reaches it.
    const MAX_SUPERTYPES: usize = 64;

    /// The supertypes the store holds for a type this file does not declare,
    /// as walk entries.
    ///
    /// Entered with no nesting path: their own supertypes come from the same
    /// relation and never from this file, which is the whole difference
    /// between a closure and a single hop.
    ///
    /// Reversed, because the caller's queue is a stack and the relation is in
    /// declaration order. §8.1.4 puts `extends` before `implements`, so the
    /// first entry is the superclass, and §8.4.8 has a superclass method beat
    /// a superinterface's default — reversing here is what pops it first, the
    /// same order the in-file walk builds by pushing the superclass last.
    fn indexed_supers(type_fqn: &str, p: &mut Probes<'_>) -> Vec<(String, Option<Vec<String>>)> {
        let Some(supers) = p.supers(type_fqn) else {
            return Vec::new();
        };
        supers
            .fqns
            .into_iter()
            .rev()
            .map(|fqn| (fqn.into_string(), None))
            .collect()
    }

    /// Every declaration of `name` this resolver can see on `owner` and the
    /// supertypes it can read.
    ///
    /// A different question from [`JavaResolver::lookup`]'s, which is "which
    /// declaration does *this site* name" and stops at the first hit because
    /// the site's arity decides. Here there is no arity to decide with, so
    /// the answer is the set, and its size is what separates "one target"
    /// from "an overload set" from "nothing of that name".
    fn name_members(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        owner: &Owner,
        name: &str,
        p: &mut Probes<'_>,
    ) -> NameGroup {
        let mut group = NameGroup {
            found: Vec::new(),
            ambiguous: false,
            unindexed: false,
        };
        let (fqn_start, local_start) = match owner {
            Owner::InRepo { fqn, local } => (fqn.clone(), local.clone()),
            _ => {
                group.unindexed = true;
                return group;
            }
        };
        let start = fqn_start.clone();
        let mut keys = vec![name.to_string()];
        for argc in 0..=Self::NAME_PROBE_ARITY {
            keys.push(fqn::arity_key(name, argc));
            keys.push(fqn::varargs_key(name, argc));
        }
        // A key a subtype declares is the one that member *is* (§8.4.8.1's
        // override), so the first type in the walk to declare it wins and the
        // supertype's declaration is not a second member.
        let mut claimed: HashSet<String> = HashSet::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: Vec<(String, Option<Vec<String>>)> = vec![(fqn_start, local_start)];
        while let Some((type_fqn, local)) = queue.pop() {
            if !seen.insert(type_fqn.clone()) {
                continue;
            }
            for key in &keys {
                if claimed.contains(key) {
                    continue;
                }
                match p.get(&fqn::member_fqn(&type_fqn, key)) {
                    Some(Entry::Definition {
                        kind: DefKind::Alias,
                        ..
                    }) => {
                        claimed.insert(key.clone());
                        group.ambiguous = true;
                    }
                    // §8.2 again: a supertype's private member is not a
                    // member of this type, so it neither claims the key nor
                    // joins the group whose size decides `AmbiguousOverload`.
                    Some(Entry::Definition { facets, .. })
                        if type_fqn != start && facets.contains(DefFacets::PRIVATE) => {}
                    Some(Entry::Definition { .. }) => {
                        claimed.insert(key.clone());
                        group
                            .found
                            .push(node_id(JavaLang::DOMAIN, &fqn::member_fqn(&type_fqn, key)));
                    }
                    _ => {}
                }
            }
            if seen.len() >= Self::MAX_SUPERTYPES {
                group.unindexed = true;
                break;
            }
            let Some(path) = local else {
                group.unindexed = true;
                queue.extend(Self::indexed_supers(&type_fqn, p));
                continue;
            };
            let Some(decl) = scope.supers.get(&path.join(&fqn::NEST.to_string())) else {
                group.unindexed = true;
                continue;
            };
            if decl.superclass.is_none() {
                group.unindexed = true;
            }
            let supers = decl.interfaces.iter().chain(decl.superclass.iter());
            for segments in supers {
                match self.canonical_type(cfg, scope, segments, p) {
                    Owner::InRepo { fqn, local } => queue.push((fqn, local)),
                    _ => group.unindexed = true,
                }
            }
        }
        group
    }

    /// The outcome a member-name group decides.
    ///
    /// One declaration is one target and resolves; two or more is the
    /// discrimination `AmbiguousOverload` names; none is the same honest miss
    /// any other member lookup reports.
    fn select_name_group(&self, group: NameGroup) -> Outcome<NodeId, String> {
        if group.ambiguous || group.found.len() > 1 {
            return Outcome::Unresolved(UnresolvedReason::AmbiguousOverload);
        }
        match group.found.into_iter().next() {
            Some(id) => Outcome::Resolved(id),
            None if group.unindexed => Outcome::Unresolved(UnresolvedReason::UnindexedSupertype),
            None => Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
        }
    }

    /// The keys a member lookup probes, most specific first.
    ///
    /// §15.12.2's phases 1 and 2 exclude variable arity entirely, so a
    /// fixed-arity declaration always beats a varargs one; among varargs, a
    /// higher minimum arity is more specific.
    fn member_keys(name: &str, argc: Option<u32>) -> Vec<String> {
        match argc {
            None => vec![name.to_string()],
            Some(n) => {
                let mut keys = Vec::with_capacity(n as usize + 2);
                keys.push(fqn::arity_key(name, n));
                for min in (0..=n).rev() {
                    keys.push(fqn::varargs_key(name, min));
                }
                keys
            }
        }
    }

    /// Look a member up on a type and, as far as this file states them, on
    /// its supertypes (H-01, I-06, H-02).
    ///
    /// The walk is superclass-first and then interfaces, which is §8.4.8's
    /// order for everything but a `static` method — the one case where a
    /// class does *not* inherit from its superinterfaces. That asymmetry is
    /// not modelled: nothing at a call site says whether the target is
    /// static, so the candidate set can only be too large, and a set that is
    /// too large ends `AmbiguousOverload` rather than at a wrong edge.
    ///
    /// A hit on a *class* is the answer as soon as it is met: every non-
    /// interface the walk can reach sits on the receiver's superclass chain,
    /// which is linear and drained before any interface, so the first one is
    /// the nearest one. A hit on an interface is not, and §9.4.1 says why —
    /// a declaration is inherited only when no subinterface in the same set
    /// redeclares it, and neither the order two interfaces are written in nor
    /// which side of a file boundary they came from decides that. Interface
    /// hits are therefore collected and then filtered by
    /// [`JavaResolver::most_specific`]; the walk stops early only on the
    /// receiver's own type, where nothing can be more specific.
    fn lookup(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        owner: &Owner,
        keys: &[String],
        arguments: Option<&[String]>,
        p: &mut Probes<'_>,
    ) -> Member {
        let (fqn_start, local_start) = match owner {
            Owner::InRepo { fqn, local } => (fqn.clone(), local.clone()),
            _ => {
                return Member::Missing { unindexed: true };
            }
        };
        if let Some(arguments) = arguments {
            let name = keys[0].split('/').next().unwrap_or(&keys[0]);
            return self.typed_lookup(
                cfg,
                scope,
                TypedSite {
                    owner: &fqn_start,
                    local: local_start,
                    name,
                    arguments,
                },
                p,
            );
        }
        let mut unindexed = false;
        let mut seen: HashSet<String> = HashSet::new();
        let mut inherited: Vec<(String, NodeId)> = Vec::new();
        let mut queue: Vec<(String, Option<Vec<String>>)> = vec![(fqn_start.clone(), local_start)];
        while let Some((type_fqn, local)) = queue.pop() {
            if !seen.insert(type_fqn.clone()) {
                continue;
            }
            for key in keys {
                match p.get(&fqn::member_fqn(&type_fqn, key)) {
                    Some(Entry::Definition {
                        kind: DefKind::Alias,
                        ..
                    }) => return Member::Ambiguous,
                    // §8.2: a private member is not inherited, so a
                    // supertype's is not a candidate here at all. Skipped and
                    // not returned — the walk carries on to whatever the
                    // subtype really can name, which is what the compiler
                    // does. On the receiver's own type it is an ordinary
                    // member and the facet says nothing.
                    Some(Entry::Definition { facets, .. })
                        if type_fqn != fqn_start && facets.contains(DefFacets::PRIVATE) => {}
                    Some(Entry::Definition { .. }) => {
                        let id = node_id(JavaLang::DOMAIN, &fqn::member_fqn(&type_fqn, key));
                        if type_fqn == fqn_start || !Self::declares_interface(&type_fqn, p) {
                            return Member::Found(id);
                        }
                        inherited.push((type_fqn.clone(), id));
                        break;
                    }
                    _ => {}
                }
            }
            if seen.len() >= Self::MAX_SUPERTYPES {
                unindexed = true;
                break;
            }
            // A type this file declares states its own supertypes; for any
            // other, the supertype phase placed them and this walks on
            // through what it placed (H-01).
            let Some(path) = local else {
                unindexed = true;
                queue.extend(Self::indexed_supers(&type_fqn, p));
                continue;
            };
            let Some(decl) = scope.supers.get(&path.join(&fqn::NEST.to_string())) else {
                unindexed = true;
                continue;
            };
            // Every class also inherits `java.lang.Object`, which is never
            // indexed; `is_object_member` answers for its members, and this
            // records that the closure is not complete.
            let mut supers: Vec<&Vec<String>> = Vec::new();
            supers.extend(decl.interfaces.iter());
            supers.extend(decl.superclass.iter());
            if decl.superclass.is_none() {
                unindexed = true;
            }
            for segments in supers {
                match self.canonical_type(cfg, scope, segments, p) {
                    Owner::InRepo { fqn, local } => queue.push((fqn, local)),
                    _ => unindexed = true,
                }
            }
        }
        match Self::most_specific(&inherited, p) {
            Some(id) => Member::Found(id),
            None => Member::Missing { unindexed },
        }
    }

    /// Apply JLS §15.12.2's strict, loose, then variable-arity phases to all
    /// visible declarations on the receiver and its indexed supertypes.
    fn typed_lookup(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        site: TypedSite<'_>,
        p: &mut Probes<'_>,
    ) -> Member {
        let mut saw_member = false;
        let mut unindexed = false;
        for phase in [
            InvocationPhase::Strict,
            InvocationPhase::Loose,
            InvocationPhase::Varargs,
        ] {
            let found = self.collect_applicable(cfg, scope, &site, phase, p);
            saw_member |= found.saw_member;
            unindexed |= found.unindexed;
            if found.ambiguous {
                return Member::Ambiguous;
            }
            if !found.candidates.is_empty() {
                return self.select_applicable(found.candidates, p);
            }
        }
        if saw_member {
            Member::Ambiguous
        } else {
            Member::Missing { unindexed }
        }
    }

    fn collect_applicable(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        site: &TypedSite<'_>,
        phase: InvocationPhase,
        p: &mut Probes<'_>,
    ) -> Applicability {
        let mut out = Applicability::default();
        let mut seen = HashSet::new();
        let mut queue = vec![(site.owner.to_string(), site.local.clone())];
        while let Some((type_fqn, local)) = queue.pop() {
            if !seen.insert(type_fqn.clone()) {
                continue;
            }
            let shapes = match phase {
                InvocationPhase::Strict | InvocationPhase::Loose => {
                    let key = fqn::arity_key(site.name, site.arguments.len() as u32);
                    let declaration = fqn::member_fqn(&type_fqn, &key);
                    let visible = Self::visible_member(&declaration, &type_fqn, site.owner, p);
                    out.saw_member |= visible;
                    let mut shapes = if visible {
                        Self::fixed_signatures(scope, site.arguments, phase)
                            .into_iter()
                            .map(|(types, depths)| (types, depths, declaration.clone()))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    if !site.arguments.is_empty()
                        && site.arguments.last().is_some_and(|ty| ty.ends_with("[]"))
                    {
                        let key = fqn::varargs_key(site.name, site.arguments.len() as u32 - 1);
                        let varargs_declaration = fqn::member_fqn(&type_fqn, &key);
                        let varargs_visible =
                            Self::visible_member(&varargs_declaration, &type_fqn, site.owner, p);
                        out.saw_member |= varargs_visible;
                        if varargs_visible {
                            let last = site.arguments.last().expect("nonempty checked above");
                            for (types, depths) in Self::fixed_signatures(
                                scope,
                                &site.arguments[..site.arguments.len() - 1],
                                phase,
                            ) {
                                for array in Self::exact_type_spellings(scope, last) {
                                    let Some(component) = array.strip_suffix("[]") else {
                                        continue;
                                    };
                                    let mut signature = types.clone();
                                    signature.push(format!("{component}..."));
                                    let mut ranks = depths.clone();
                                    ranks.push(0);
                                    shapes.push((signature, ranks, varargs_declaration.clone()));
                                }
                            }
                        }
                    }
                    shapes
                }
                InvocationPhase::Varargs => {
                    let mut shapes = Vec::new();
                    for min in (0..=site.arguments.len() as u32).rev() {
                        let key = fqn::varargs_key(site.name, min);
                        let declaration = fqn::member_fqn(&type_fqn, &key);
                        let visible = Self::visible_member(&declaration, &type_fqn, site.owner, p);
                        out.saw_member |= visible;
                        if visible {
                            if min as usize == site.arguments.len() {
                                for (prefix, depths) in Self::fixed_signatures(
                                    scope,
                                    site.arguments,
                                    InvocationPhase::Loose,
                                ) {
                                    let prefix_fqn = fqn::member_fqn(
                                        &type_fqn,
                                        &fqn::varargs_prefix_key(site.name, &prefix),
                                    );
                                    match p.get(&prefix_fqn) {
                                        Some(Entry::Alias { target }) => {
                                            out.candidates.push(Applicable {
                                                owner: type_fqn.clone(),
                                                target,
                                                depths,
                                            });
                                        }
                                        Some(Entry::Definition {
                                            kind: DefKind::Alias,
                                            ..
                                        }) => out.ambiguous = true,
                                        _ => {}
                                    }
                                }
                                continue;
                            }
                            shapes.extend(
                                Self::varargs_signatures(scope, site.arguments, min as usize)
                                    .into_iter()
                                    .map(|(types, depths)| (types, depths, declaration.clone())),
                            );
                        }
                    }
                    shapes
                }
            };
            for (types, depths, declaration) in shapes {
                let signature = fqn::signature_key(site.name, &types);
                let signature_fqn = fqn::member_fqn(&type_fqn, &signature);
                let Some(entry) = p.get(&signature_fqn) else {
                    continue;
                };
                let target = match entry {
                    Entry::Alias { target } => target,
                    Entry::Definition {
                        facets,
                        kind: DefKind::Method | DefKind::Constructor,
                    } if type_fqn == site.owner || !facets.contains(DefFacets::PRIVATE) => {
                        node_id(JavaLang::DOMAIN, &signature_fqn)
                    }
                    _ => continue,
                };
                // A forwarding signature for a unique callable must point at
                // the declaration key this phase discovered.
                if matches!(entry, Entry::Alias { .. })
                    && !matches!(p.get(&declaration), Some(Entry::Definition { .. }))
                {
                    continue;
                }
                out.candidates.push(Applicable {
                    owner: type_fqn.clone(),
                    target,
                    depths,
                });
            }
            if seen.len() >= Self::MAX_SUPERTYPES {
                out.unindexed = true;
                break;
            }
            let Some(path) = local else {
                out.unindexed = true;
                queue.extend(Self::indexed_supers(&type_fqn, p));
                continue;
            };
            let Some(decl) = scope.supers.get(&path.join(&fqn::NEST.to_string())) else {
                out.unindexed = true;
                continue;
            };
            if decl.superclass.is_none() {
                out.unindexed = true;
            }
            let supers = decl.interfaces.iter().chain(decl.superclass.iter());
            for segments in supers {
                match self.canonical_type(cfg, scope, segments, p) {
                    Owner::InRepo { fqn, local } => queue.push((fqn, local)),
                    _ => out.unindexed = true,
                }
            }
        }
        out
    }

    fn visible_member(
        declaration: &str,
        declaration_owner: &str,
        start_owner: &str,
        p: &mut Probes<'_>,
    ) -> bool {
        match p.get(declaration) {
            Some(Entry::Definition { facets, .. })
                if declaration_owner != start_owner && facets.contains(DefFacets::PRIVATE) =>
            {
                false
            }
            Some(Entry::Definition { .. }) => true,
            _ => false,
        }
    }

    fn fixed_signatures(
        scope: &JavaScope,
        arguments: &[String],
        phase: InvocationPhase,
    ) -> Vec<(Vec<String>, Vec<u8>)> {
        let alternatives = arguments
            .iter()
            .map(|argument| Self::conversion_targets(scope, argument, phase))
            .collect::<Vec<_>>();
        Self::signature_product(&alternatives)
    }

    fn varargs_signatures(
        scope: &JavaScope,
        arguments: &[String],
        min: usize,
    ) -> Vec<(Vec<String>, Vec<u8>)> {
        if min > arguments.len() || min == arguments.len() {
            // With no repeated argument the component type is not present at
            // the site, so this key is known but its applicability is not.
            return Vec::new();
        }
        let options = arguments
            .iter()
            .map(|argument| Self::conversion_targets(scope, argument, InvocationPhase::Loose))
            .collect::<Vec<_>>();
        let prefixes = Self::signature_product(&options[..min]);
        let mut common: HashMap<String, Vec<u8>> = HashMap::new();
        for (ty, depth) in &options[min] {
            common.insert(ty.clone(), vec![*depth]);
        }
        for choices in &options[min + 1..] {
            common.retain(|ty, depths| {
                let Some((_, depth)) = choices.iter().find(|(candidate, _)| candidate == ty) else {
                    return false;
                };
                depths.push(*depth);
                true
            });
        }
        let mut out = Vec::new();
        for (prefix_types, prefix_depths) in prefixes {
            for (component, tail_depths) in &common {
                if out.len() >= 64 {
                    return Vec::new();
                }
                let mut types = prefix_types.clone();
                types.push(format!("{component}..."));
                let mut depths = prefix_depths.clone();
                depths.extend(tail_depths);
                out.push((types, depths));
            }
        }
        out
    }

    fn signature_product(alternatives: &[Vec<(String, u8)>]) -> Vec<(Vec<String>, Vec<u8>)> {
        let mut signatures = vec![(Vec::new(), Vec::new())];
        for choices in alternatives {
            let mut next = Vec::new();
            for (prefix, depths) in &signatures {
                for (choice, depth) in choices {
                    if next.len() >= 64 {
                        return Vec::new();
                    }
                    let mut signature = prefix.clone();
                    signature.push(choice.clone());
                    let mut ranks = depths.clone();
                    ranks.push(*depth);
                    next.push((signature, ranks));
                }
            }
            signatures = next;
        }
        signatures
    }

    /// The fixture-proven conversion surface. No user-defined subtype is
    /// guessed: only primitive widening, boxing, unboxing plus widening, and
    /// the built-in wrapper-to-`Number`/`Object` chains are represented.
    fn conversion_targets(
        scope: &JavaScope,
        argument: &str,
        phase: InvocationPhase,
    ) -> Vec<(String, u8)> {
        let mut targets = Vec::new();
        for spelling in Self::exact_type_spellings(scope, argument) {
            let simple = spelling
                .strip_prefix("java.lang.")
                .filter(|name| JAVA_LANG.binary_search(name).is_ok())
                .unwrap_or(&spelling);
            targets.push((simple.to_string(), 0));
            if simple != spelling {
                targets.push((spelling.clone(), 0));
            } else if JAVA_LANG.binary_search(&simple).is_ok() {
                targets.push((format!("java.lang.{simple}"), 0));
            }
            Self::push_strict_widening(simple, &mut targets);
            if phase != InvocationPhase::Strict {
                Self::push_loose_conversions(simple, &mut targets);
            }
        }
        let mut seen = HashSet::new();
        targets.retain(|target| seen.insert(target.0.clone()));
        targets
    }

    /// Exact source spellings justified by this compilation unit.
    ///
    /// This is alias expansion, not subtype inference: only a single-type
    /// import or the declared package may equate a simple name with a
    /// qualified one. Wildcard imports and arbitrary suffix matches are not
    /// guessed. Array and varargs markers remain attached to the aliased base.
    fn exact_type_spellings(scope: &JavaScope, argument: &str) -> Vec<String> {
        let mut base = argument;
        let mut suffix = String::new();
        loop {
            if let Some(stripped) = base.strip_suffix("[]") {
                base = stripped;
                suffix.insert_str(0, "[]");
            } else if let Some(stripped) = base.strip_suffix("...") {
                base = stripped;
                suffix.insert_str(0, "...");
            } else {
                break;
            }
        }

        let decorate = |name: &str| format!("{name}{suffix}");
        let mut spellings = vec![argument.to_string()];
        if matches!(
            base,
            "byte" | "short" | "char" | "int" | "long" | "float" | "double" | "boolean" | "void"
        ) {
            return spellings;
        }
        if !base.contains('.') {
            if let Some(imported) = scope.single_type.get(base) {
                spellings.push(decorate(&imported.join(".")));
            } else if !scope.container.is_empty() && !scope.container.starts_with("module:") {
                spellings.push(decorate(&format!("{}.{}", scope.container, base)));
            }
        } else if let Some(simple) = base.rsplit('.').next() {
            let imported_here = scope
                .single_type
                .get(simple)
                .is_some_and(|imported| imported.join(".") == base);
            let package_local = !scope.container.is_empty()
                && !scope.container.starts_with("module:")
                && base
                    .strip_suffix(simple)
                    .is_some_and(|prefix| prefix.strip_suffix('.') == Some(&scope.container));
            if imported_here || package_local {
                spellings.push(decorate(simple));
            }
        }
        spellings.sort();
        spellings.dedup();
        if let Some(at) = spellings.iter().position(|spelling| spelling == argument) {
            spellings.swap(0, at);
        }
        spellings
    }

    fn push_strict_widening(argument: &str, targets: &mut Vec<(String, u8)>) {
        let primitive: &[&str] = match argument {
            "byte" => &["short", "int", "long", "float", "double"],
            "short" | "char" => &["int", "long", "float", "double"],
            "int" => &["long", "float", "double"],
            "long" => &["float", "double"],
            "float" => &["double"],
            _ => &[],
        };
        targets.extend(
            primitive
                .iter()
                .enumerate()
                .map(|(depth, ty)| ((*ty).to_string(), depth as u8 + 1)),
        );
        let references: &[&str] = match argument {
            "Byte" | "Short" | "Integer" | "Long" | "Float" | "Double" => &["Number", "Object"],
            "Number" | "Character" | "Boolean" => &["Object"],
            _ => &[],
        };
        targets.extend(
            references
                .iter()
                .enumerate()
                .map(|(depth, ty)| ((*ty).to_string(), depth as u8 + 1)),
        );
    }

    fn push_loose_conversions(argument: &str, targets: &mut Vec<(String, u8)>) {
        let boxed = match argument {
            "byte" => Some("Byte"),
            "short" => Some("Short"),
            "char" => Some("Character"),
            "int" => Some("Integer"),
            "long" => Some("Long"),
            "float" => Some("Float"),
            "double" => Some("Double"),
            "boolean" => Some("Boolean"),
            _ => None,
        };
        if let Some(wrapper) = boxed {
            targets.push((wrapper.to_string(), 1));
            let chain: &[&str] = match wrapper {
                "Byte" | "Short" | "Integer" | "Long" | "Float" | "Double" => &["Number", "Object"],
                _ => &["Object"],
            };
            targets.extend(
                chain
                    .iter()
                    .enumerate()
                    .map(|(depth, ty)| ((*ty).to_string(), depth as u8 + 2)),
            );
        }
        let unboxed = match argument {
            "Byte" => Some("byte"),
            "Short" => Some("short"),
            "Character" => Some("char"),
            "Integer" => Some("int"),
            "Long" => Some("long"),
            "Float" => Some("float"),
            "Double" => Some("double"),
            "Boolean" => Some("boolean"),
            _ => None,
        };
        if let Some(primitive) = unboxed {
            targets.push((primitive.to_string(), 1));
            let mut widening = Vec::new();
            Self::push_strict_widening(primitive, &mut widening);
            targets.extend(
                widening
                    .into_iter()
                    .filter(|(ty, _)| ty != primitive)
                    .map(|(ty, depth)| (ty, depth + 1)),
            );
        }
    }

    fn select_applicable(&self, candidates: Vec<Applicable>, p: &mut Probes<'_>) -> Member {
        let mut unique: Vec<Applicable> = Vec::new();
        for candidate in candidates {
            if let Some(held) = unique
                .iter_mut()
                .find(|held| held.target == candidate.target)
            {
                if Self::depths_dominate(&candidate.depths, &held.depths) {
                    *held = candidate;
                }
            } else {
                unique.push(candidate);
            }
        }
        let survivors = unique
            .iter()
            .filter(|candidate| {
                !unique.iter().any(|other| {
                    other.target != candidate.target
                        && Self::depths_dominate(&other.depths, &candidate.depths)
                })
            })
            .collect::<Vec<_>>();
        let concrete = survivors
            .iter()
            .copied()
            .filter(|candidate| !Self::declares_interface(&candidate.owner, p))
            .collect::<Vec<_>>();
        let ranked = if concrete.is_empty() {
            survivors
        } else {
            concrete
        };
        if let [only] = ranked.as_slice() {
            return Member::Found(only.target);
        }
        let owner_winner = ranked.iter().find(|candidate| {
            ranked.iter().all(|other| {
                candidate.target == other.target
                    || (candidate.owner != other.owner
                        && Self::reaches_supertype(&candidate.owner, &other.owner, p))
            })
        });
        owner_winner.map_or(Member::Ambiguous, |winner| Member::Found(winner.target))
    }

    fn depths_dominate(left: &[u8], right: &[u8]) -> bool {
        left.len() == right.len()
            && left.iter().zip(right).all(|(left, right)| left <= right)
            && left.iter().zip(right).any(|(left, right)| left < right)
    }

    /// The one interface declaration a class actually inherits (§9.4.1).
    ///
    /// A declaration is struck out when some *other* interface that declared
    /// the same member extends it: that subinterface overrides it, and only
    /// the override is inherited. `C implements Alpha, Beta` with
    /// `Beta extends Alpha` therefore answers `Beta`, whichever order the
    /// two are written in and whichever side of a file boundary they were
    /// read from — `javac` on that tree agrees.
    ///
    /// Several may survive: unrelated interfaces declaring one signature is
    /// legal, and §15.12.2.1 leaves the compiler to pick among declarations
    /// that are equally specific. Walk order picks here, for the same reason
    /// it always did — the call really does reach a body, and reporting it
    /// unresolvable would trade a slightly-wrong edge for no edge at all.
    fn most_specific(hits: &[(String, NodeId)], p: &mut Probes<'_>) -> Option<NodeId> {
        let (_, first) = hits.first()?;
        if hits.len() == 1 {
            return Some(*first);
        }
        for (type_fqn, id) in hits {
            let overridden = hits
                .iter()
                .any(|(other, _)| other != type_fqn && Self::reaches_supertype(other, type_fqn, p));
            if !overridden {
                return Some(*id);
            }
        }
        Some(*first)
    }

    /// Whether `sub` reaches `sup` through the stored supertype relation.
    ///
    /// Bounded by [`JavaResolver::MAX_SUPERTYPES`] like every other closure
    /// walk here, and for the same reason: a cut walk answers `false`, which
    /// keeps the earlier hit rather than inventing a later one.
    fn reaches_supertype(sub: &str, sup: &str, p: &mut Probes<'_>) -> bool {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue = vec![sub.to_string()];
        while let Some(type_fqn) = queue.pop() {
            if seen.len() >= Self::MAX_SUPERTYPES {
                return false;
            }
            if !seen.insert(type_fqn.clone()) {
                continue;
            }
            for (fqn, _) in Self::indexed_supers(&type_fqn, p) {
                if fqn == sup {
                    return true;
                }
                queue.push(fqn);
            }
        }
        false
    }

    /// Whether a type this repository declares is an interface (§9.1),
    /// annotation types included (§9.6).
    ///
    /// Probed rather than inferred, so the read is logged: an outcome that
    /// depends on this facet has to be woken when the facet moves, which is
    /// what carrying it in [`crate::store::NodePayload`] arranges.
    fn declares_interface(type_fqn: &str, p: &mut Probes<'_>) -> bool {
        matches!(
            p.get(type_fqn),
            Some(Entry::Definition { facets, .. }) if facets.contains(DefFacets::INTERFACE)
        )
    }

    /// Turn a member lookup into an outcome, with the honest miss.
    fn select(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        owner: Owner,
        name: &str,
        invocation: Invocation<'_>,
        p: &mut Probes<'_>,
    ) -> Outcome<NodeId, String> {
        match owner {
            Owner::Failed(reason) => Outcome::Unresolved(reason),
            Owner::Outside(package) => Outcome::External(package),
            owner @ Owner::InRepo { .. } => {
                let keys = Self::member_keys(name, invocation.argc);
                match self.lookup(cfg, scope, &owner, &keys, invocation.arguments, p) {
                    Member::Found(id) => Outcome::Resolved(id),
                    Member::Ambiguous => Outcome::Unresolved(UnresolvedReason::AmbiguousOverload),
                    Member::Missing { unindexed } => {
                        if is_object_member(name, invocation.argc) {
                            // Every class inherits it (§4.3.2), and `Object`
                            // is never a definition in this repository.
                            Outcome::External(JAVA_LANG_PACKAGE.to_string())
                        } else if unindexed {
                            // B-05: the member exists somewhere above, and we
                            // never attributed that somewhere to a package.
                            Outcome::Unresolved(UnresolvedReason::UnindexedSupertype)
                        } else {
                            // The closure was complete and the name is absent.
                            // In a corpus that compiles this is *our* bug.
                            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
                        }
                    }
                }
            }
        }
    }

    /// Look a member up in an erased type frame: what it declares itself
    /// first, then the supertypes it names (T-03..T-05).
    ///
    /// Its own declarations come first because they shadow, and because a hit
    /// there is the finding this whole path exists for: the target is a real
    /// member of a type with no canonical name (§6.7), so the honest answer
    /// is that there is nothing to link to — never a same-named member of the
    /// class the frame happens to sit in.
    fn frame_lookup(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        frame: &ErasedType,
        keys: &[String],
        arguments: Option<&[String]>,
        p: &mut Probes<'_>,
    ) -> FrameMember {
        if keys.iter().any(|key| frame.members.contains(key)) {
            return FrameMember::Own;
        }
        // §8.1.4: no `extends` clause means `java.lang.Object`, which is
        // never indexed — so the closure is incomplete before it starts.
        let mut unindexed = frame.superclass.is_none();
        let supers = frame.superclass.iter().chain(frame.interfaces.iter());
        for segments in supers {
            let owner = self.canonical_type(cfg, scope, segments, p);
            if !matches!(owner, Owner::InRepo { .. }) {
                unindexed = true;
                continue;
            }
            match self.lookup(cfg, scope, &owner, keys, arguments, p) {
                Member::Found(id) => return FrameMember::Found(id),
                Member::Ambiguous => return FrameMember::Ambiguous,
                Member::Missing { unindexed: more } => unindexed |= more,
            }
        }
        FrameMember::Missing { unindexed }
    }

    /// The outcome of a lookup that ended inside an erased frame and may not
    /// continue outward.
    fn frame_outcome(
        &self,
        found: FrameMember,
        name: &str,
        argc: Option<u32>,
        unindexed: bool,
    ) -> Outcome<NodeId, String> {
        match found {
            // The same judgement the node rule already makes for the local
            // class itself: by design not a node, policy-caused rather than a
            // language-support failure, and reported beside `External`.
            FrameMember::Own => Outcome::Unresolved(UnresolvedReason::LocalBinding),
            FrameMember::Found(id) => Outcome::Resolved(id),
            FrameMember::Ambiguous => Outcome::Unresolved(UnresolvedReason::AmbiguousOverload),
            FrameMember::Missing { unindexed: more } => {
                if is_object_member(name, argc) {
                    Outcome::External(JAVA_LANG_PACKAGE.to_string())
                } else if unindexed || more {
                    Outcome::Unresolved(UnresolvedReason::UnindexedSupertype)
                } else {
                    Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
                }
            }
        }
    }

    /// The type a reference sits inside, innermost last.
    fn enclosing_types(r: &Reference) -> Vec<String> {
        match &r.enclosing {
            Some(e) if e.kind == DefKind::Type => e.path.clone(),
            Some(e) => e.path[..e.path.len().saturating_sub(1)].to_vec(),
            None => Vec::new(),
        }
    }

    /// The owner a `this`, `super` or unqualified reference starts from.
    fn enclosing_owner(
        &self,
        scope: &JavaScope,
        path: &[String],
        p: &mut Probes<'_>,
    ) -> Option<Owner> {
        if path.is_empty() {
            return None;
        }
        self.probe_type(scope, &scope.container, path, Some(path.to_vec()), p)
    }

    /// Resolve a reference whose target is a type name (N-03).
    fn resolve_type_ref(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        r: &Reference,
        p: &mut Probes<'_>,
    ) -> Outcome<NodeId, String> {
        let enclosing = Self::enclosing_types(r);
        let segments = &r.target.segments;
        let owner = if segments.len() == 1 {
            self.simple_type(cfg, scope, &segments[0], &enclosing, p)
                .unwrap_or_else(|| Owner::Failed(self.simple_type_miss(cfg, scope)))
        } else {
            self.canonical_type(cfg, scope, segments, p)
        };
        match owner {
            Owner::InRepo { fqn, .. } => Outcome::Resolved(node_id(JavaLang::DOMAIN, &fqn)),
            Owner::Outside(package) => Outcome::External(package),
            Owner::Failed(reason) => Outcome::Unresolved(reason),
        }
    }

    /// Resolve a call, a creation site, a field access or a method reference.
    fn resolve_member_ref(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        r: &Reference,
        p: &mut Probes<'_>,
    ) -> Outcome<NodeId, String> {
        let enclosing = Self::enclosing_types(r);
        let site = Site {
            types: &enclosing,
            at: r.span.byte_start,
        };
        let arguments = r.arg_types.as_deref();
        let invocation = Invocation {
            argc: r.argc,
            arguments,
        };
        let segments = &r.target.segments;

        // C-03: `this(…)` and `super(…)` name a constructor exactly, and
        // C-01's creation site names one on the type it wrote.
        if r.kind == RefKind::New {
            return self.resolve_new(cfg, scope, r, site, p);
        }
        let Some((name, qualifier)) = segments.split_last() else {
            return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition);
        };
        // C-08 / X-05: a method reference's overload is chosen by the target
        // functional-interface type (§15.13.1). The owner and the name are
        // known and the discriminator is not — which is exactly what
        // `AmbiguousOverload` is defined to cover.
        let argc = r.argc;
        let owner = match &r.target.root {
            TargetRoot::Expr => {
                // X-01: the operand is an expression, not a name.
                return Outcome::Unresolved(UnresolvedReason::NeedsExpressionType);
            }
            TargetRoot::This { qualifier: outer } => {
                // §15.8.3: an unqualified `this` denotes the innermost
                // enclosing *instance*, and inside an anonymous or local
                // class that is the frame's — never the class around it. It
                // also never walks outward, which is what separates this from
                // the unqualified-invocation path below.
                if outer.is_empty()
                    && qualifier.is_empty()
                    && let Some(frame) = scope.erased_at(site.at).first()
                {
                    let keys = Self::member_keys(name, argc);
                    let found = self.frame_lookup(cfg, scope, frame, &keys, arguments, p);
                    return self.frame_outcome(found, name, argc, false);
                }
                let path = self.this_path(scope, outer, site.types);
                match self.enclosing_owner(scope, &path, p) {
                    Some(owner) => owner,
                    None => return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
                }
            }
            TargetRoot::Super { qualifier: outer } => {
                match self.super_owner(cfg, scope, outer, site, p) {
                    Some(owner) => owner,
                    None => return Outcome::External(JAVA_LANG_PACKAGE.to_string()),
                }
            }
            TargetRoot::Name if qualifier.is_empty() => {
                // N-02: an unqualified invocation. §6.5.1 makes a bare
                // `m(…)` a MethodName and nothing else, so no local can
                // shadow it and the search is purely the enclosing chain.
                return self.unqualified(cfg, scope, name, invocation, site, p);
            }
            TargetRoot::Name => {
                let (owner, consumed) = self.qualifier(cfg, scope, qualifier, site, p);
                if consumed < qualifier.len() {
                    return match owner {
                        Owner::Outside(package) => Outcome::External(package),
                        Owner::Failed(reason) => Outcome::Unresolved(reason),
                        // A member of a placed type, selected out of again:
                        // its own type is stated in another file.
                        Owner::InRepo { .. } => {
                            Outcome::Unresolved(UnresolvedReason::NeedsTypeInference)
                        }
                    };
                }
                owner
            }
        };
        // A `this.f.m()` or `super.f.m()` chain selects a field first. Its
        // declared type is written in this file whenever the field is (X-02),
        // and `f.m()` one line away already reads it — so giving up here was
        // reporting "the receiver is a name with no declared type" about a
        // name whose type is on line 3.
        if !matches!(r.target.root, TargetRoot::Name)
            && let Some(field) = qualifier.first()
        {
            // Two selections deep the first one's *own* type would have to be
            // computed, which is X-01's territory.
            if qualifier.len() > 1 {
                return Outcome::Unresolved(UnresolvedReason::NeedsTypeInference);
            }
            let Some(declared) = scope
                .field_at(field, site.at)
                .and_then(|binding| binding.declared_type.clone())
            else {
                return Outcome::Unresolved(UnresolvedReason::NeedsTypeInference);
            };
            let owner = self.declared_owner(cfg, scope, &declared, site.at, 0, p);
            return self.select(cfg, scope, owner, name, invocation, p);
        }
        if r.kind == RefKind::MethodRef {
            // C-08 / X-05: the overload is chosen by the target
            // functional-interface type (§15.13.1), which is not at this
            // site — but a singleton needs no choosing, and reporting one as
            // `AmbiguousOverload` says "we compared candidates and could not
            // choose" about a set that was never looked at.
            return match owner {
                Owner::Failed(reason) => Outcome::Unresolved(reason),
                Owner::Outside(package) => Outcome::External(package),
                owner @ Owner::InRepo { .. } => {
                    let group = self.name_members(cfg, scope, &owner, name, p);
                    self.select_name_group(group)
                }
            };
        }
        self.select(cfg, scope, owner, name, invocation, p)
    }

    /// The type path a `this` or `Outer.this` names (§15.8.4).
    fn this_path(&self, scope: &JavaScope, outer: &[String], enclosing: &[String]) -> Vec<String> {
        if outer.is_empty() {
            return enclosing.to_vec();
        }
        // `Outer.this` names the enclosing instance of the type written.
        let wanted = outer.last().cloned().unwrap_or_default();
        match enclosing.iter().position(|segment| *segment == wanted) {
            Some(at) => enclosing[..=at].to_vec(),
            None => scope
                .file_types
                .get(&wanted)
                .and_then(|paths| paths.first().cloned())
                .unwrap_or_else(|| enclosing.to_vec()),
        }
    }

    /// The type a `super` or `Iface.super` reference starts from (H-03).
    ///
    /// `None` when the class declares no `extends` clause: its superclass is
    /// `java.lang.Object` (§8.1.4), which is never indexed.
    ///
    /// "The class" is the one *immediately* enclosing the site (§15.11.2), so
    /// an erased frame containing the site answers before the named chain
    /// does: `super.m()` inside `new Base(){…}` names a member of `Base`, and
    /// reading the enclosing named class's `extends` clause there produces an
    /// edge to a method on an unrelated type.
    fn super_owner(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        outer: &[String],
        site: Site<'_>,
        p: &mut Probes<'_>,
    ) -> Option<Owner> {
        if !outer.is_empty() {
            // `Iface.super.m()` targets exactly `Iface#m` — no ambiguity and
            // no inference.
            return Some(self.canonical_type(cfg, scope, outer, p));
        }
        if let Some(frame) = scope.erased_at(site.at).first() {
            let segments = frame.superclass.clone()?;
            return Some(self.canonical_type(cfg, scope, &segments, p));
        }
        let decl = scope.supers.get(&site.types.join(&fqn::NEST.to_string()))?;
        let segments = decl.superclass.clone()?;
        Some(self.canonical_type(cfg, scope, &segments, p))
    }

    /// N-02's ordered candidate list for an unqualified invocation.
    fn unqualified(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        name: &str,
        invocation: Invocation<'_>,
        site: Site<'_>,
        p: &mut Probes<'_>,
    ) -> Outcome<NodeId, String> {
        let keys = Self::member_keys(name, invocation.argc);
        let enclosing = site.types;
        let mut unindexed = enclosing.is_empty();
        // 0: the erased type frames the site sits in, innermost first. They
        // are type declarations §15.12.1 searches before anything lexically
        // outside them, and they are not in `enclosing` because they are not
        // nodes (T-03..T-05).
        for frame in scope.erased_at(site.at) {
            match self.frame_lookup(cfg, scope, frame, &keys, invocation.arguments, p) {
                FrameMember::Own => {
                    return Outcome::Unresolved(UnresolvedReason::LocalBinding);
                }
                FrameMember::Found(id) => return Outcome::Resolved(id),
                FrameMember::Ambiguous => {
                    return Outcome::Unresolved(UnresolvedReason::AmbiguousOverload);
                }
                FrameMember::Missing { unindexed: more } => {
                    unindexed |= more;
                    // §15.12.1 picks the innermost enclosing type declaration
                    // of which a method of that *name* is a member and stops
                    // there; applicability is decided afterwards. So a frame
                    // declaring the name at another arity still ends the
                    // search, and walking outward past it would link to a
                    // type this site is not in.
                    if frame.member_names.contains(name) {
                        return self.frame_outcome(
                            FrameMember::Missing { unindexed: more },
                            name,
                            invocation.argc,
                            unindexed,
                        );
                    }
                }
            }
        }
        // 1 and 2: the innermost enclosing type and its supertype closure,
        // then each lexically enclosing type outward (§15.12.1).
        for depth in (1..=enclosing.len()).rev() {
            let path = &enclosing[..depth];
            let Some(owner) = self.enclosing_owner(scope, path, p) else {
                unindexed = true;
                continue;
            };
            match self.lookup(cfg, scope, &owner, &keys, invocation.arguments, p) {
                Member::Found(id) => return Outcome::Resolved(id),
                Member::Ambiguous => {
                    return Outcome::Unresolved(UnresolvedReason::AmbiguousOverload);
                }
                Member::Missing { unindexed: more } => unindexed |= more,
            }
        }
        // 3 and 4: single-static-import owners, then static-import-on-demand
        // owners (§7.5.3, §7.5.4). Each tier aggregates all of its matching
        // owners, but an applicable group in tier 3 ends the search before
        // tier 4 can contribute.
        let single_statics = scope
            .single_static
            .iter()
            .filter(|(member, _)| member == name)
            .map(|(_, owner)| owner.clone())
            .collect::<Vec<_>>();
        if let Some(outcome) = self.unqualified_import_tier(
            cfg,
            scope,
            &single_statics,
            &keys,
            invocation.arguments,
            &mut unindexed,
            p,
        ) {
            return outcome;
        }
        if let Some(outcome) = self.unqualified_import_tier(
            cfg,
            scope,
            &scope.static_on_demand,
            &keys,
            invocation.arguments,
            &mut unindexed,
            p,
        ) {
            return outcome;
        }
        if is_object_member(name, invocation.argc) {
            return Outcome::External(JAVA_LANG_PACKAGE.to_string());
        }
        if unindexed {
            Outcome::Unresolved(UnresolvedReason::UnindexedSupertype)
        } else {
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
        }
    }

    fn unqualified_import_tier(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        owners: &[Vec<String>],
        keys: &[String],
        arguments: Option<&[String]>,
        unindexed: &mut bool,
        p: &mut Probes<'_>,
    ) -> Option<Outcome<NodeId, String>> {
        let mut imported = Vec::new();
        let mut imported_external = None;
        let mut imported_ambiguous = false;
        for segments in owners {
            let owner = self.canonical_type(cfg, scope, segments, p);
            match &owner {
                Owner::Outside(package) => {
                    imported_external.get_or_insert_with(|| package.clone());
                    continue;
                }
                Owner::Failed(_) => {
                    *unindexed = true;
                    continue;
                }
                Owner::InRepo { .. } => {}
            }
            match self.lookup(cfg, scope, &owner, keys, arguments, p) {
                Member::Found(id) => imported.push(id),
                Member::Ambiguous => imported_ambiguous = true,
                Member::Missing { unindexed: more } => *unindexed |= more,
            }
        }
        imported.sort_unstable();
        imported.dedup();
        if imported_ambiguous
            || imported.len() > 1
            || (!imported.is_empty() && imported_external.is_some())
        {
            return Some(Outcome::Unresolved(UnresolvedReason::AmbiguousOverload));
        }
        if let Some(id) = imported.into_iter().next() {
            return Some(Outcome::Resolved(id));
        }
        if let Some(package) = imported_external {
            return Some(Outcome::External(package));
        }
        None
    }

    /// C-01, C-03, C-04: an object creation site names a constructor.
    fn resolve_new(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        r: &Reference,
        site: Site<'_>,
        p: &mut Probes<'_>,
    ) -> Outcome<NodeId, String> {
        let owner = match &r.target.root {
            TargetRoot::This { .. } => {
                // A local class's own constructor, which §6.7 gives no
                // canonical name and this graph therefore no node.
                if !scope.erased_at(site.at).is_empty() {
                    return Outcome::Unresolved(UnresolvedReason::LocalBinding);
                }
                match self.enclosing_owner(scope, site.types, p) {
                    Some(owner) => owner,
                    None => return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
                }
            }
            TargetRoot::Super { .. } => {
                // §8.8.7: `super(…)` names the superclass's constructor, and
                // a class with no `extends` clause extends `java.lang.Object`.
                match self.super_owner(cfg, scope, &[], site, p) {
                    Some(owner) => owner,
                    None => return Outcome::External(JAVA_LANG_PACKAGE.to_string()),
                }
            }
            TargetRoot::Expr => return Outcome::Unresolved(UnresolvedReason::NeedsExpressionType),
            TargetRoot::Name => {
                if r.target.segments.len() == 1 {
                    self.simple_type(cfg, scope, &r.target.segments[0], site.types, p)
                        .unwrap_or_else(|| Owner::Failed(self.simple_type_miss(cfg, scope)))
                } else {
                    self.canonical_type(cfg, scope, &r.target.segments, p)
                }
            }
        };
        // C-05 / §15.9.5.1: `new Iface(){…}` declares an anonymous class that
        // *implements* the interface and extends `Object`, so the constructor
        // it invokes is `Object#<init>()` and never one of `Iface`'s.
        //
        // Which of the two shapes this is turns on a property of the named
        // type, so it is read off the type: [`DefFacets::INTERFACE`], which
        // the store now carries. The rule used to be inferred from the search
        // instead — "every in-repo class carries a constructor (D-10
        // synthesizes §8.8.9's implicit one), so a missing constructor means
        // an interface" — and that inference is false for every other way the
        // lookup can miss. `new Base(1){…}` against a class with no
        // one-argument constructor, or a creation of a name nothing places at
        // all, were both answered `java.lang`: a claim about a package,
        // resting on the resolver's own failure to find something.
        //
        // Read before `select` consumes the owner, and only for a site that
        // writes a class body: a creation with no body cannot be C-05 at all,
        // and probing anyway would put an identity in this reference's
        // candidate set that its outcome does not depend on.
        let on_interface = scope
            .anonymous_body_at(r.span.byte_start, r.span.byte_end)
            .is_some()
            && self.interface_owner(&owner, p);
        let settled = self.select(
            cfg,
            scope,
            owner,
            fqn::INIT,
            Invocation {
                argc: r.argc,
                arguments: r.arg_types.as_deref(),
            },
            p,
        );
        if on_interface
            && matches!(
                settled,
                Outcome::Unresolved(
                    UnresolvedReason::UnindexedSupertype | UnresolvedReason::NoMatchingDefinition
                )
            )
        {
            return Outcome::External(JAVA_LANG_PACKAGE.to_string());
        }
        settled
    }

    /// Whether a placed owner is a type this repository declares *as an
    /// interface* (§9.1), annotation types included (§9.6).
    ///
    /// Probed rather than carried on [`Owner`] so the read is logged: the
    /// outcome now depends on this identity's facets, and a reference that
    /// read them has to be woken when they move — which is exactly what
    /// putting them in [`crate::store::NodePayload`] arranges. A type outside
    /// the repository answers `false`, because nothing in this graph says what
    /// it is and guessing is what this replaces.
    fn interface_owner(&self, owner: &Owner, p: &mut Probes<'_>) -> bool {
        let Owner::InRepo { fqn, .. } = owner else {
            return false;
        };
        matches!(
            p.get(fqn),
            Some(Entry::Definition { facets, .. }) if facets.contains(DefFacets::INTERFACE)
        )
    }

    /// I-01 … I-08: every import form, plus a module directive.
    fn resolve_import(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        r: &Reference,
        p: &mut Probes<'_>,
    ) -> Outcome<NodeId, String> {
        let segments = &r.target.segments;
        if segments.is_empty() {
            return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition);
        }
        let dotted = segments.join(".");
        let raw = r.raw_target.as_str();
        let on_demand = raw.ends_with(".*");
        // I-07 / §7.7.1: a module import and a `requires` directive both name
        // a module, which P-05 makes a node of its own.
        if raw.starts_with("module ")
            || (r.space == crate::model::DeclSpace::Namespace && !on_demand)
        {
            let fqn = fqn::container("", Some(&dotted));
            return match p.get(&fqn) {
                Some(_) => Outcome::Resolved(node_id(JavaLang::DOMAIN, &fqn)),
                None => Outcome::External(format!("module:{dotted}")),
            };
        }
        // I-02 / §7.5.2: a type-import-on-demand names a package.
        if on_demand && r.space == crate::model::DeclSpace::Namespace {
            return match p.get(&dotted) {
                Some(Entry::Container) => Outcome::Resolved(node_id(JavaLang::DOMAIN, &dotted)),
                _ => Outcome::External(outside(&dotted)),
            };
        }
        // I-05 / §7.5.4: a static-import-on-demand names an owner *type*.
        if on_demand {
            return match self.canonical_type(cfg, scope, segments, p) {
                Owner::InRepo { fqn, .. } => Outcome::Resolved(node_id(JavaLang::DOMAIN, &fqn)),
                Owner::Outside(package) => Outcome::External(package),
                Owner::Failed(reason) => Outcome::Unresolved(reason),
            };
        }
        // I-01 / §7.5.1: a single-type import names a type.
        if r.space == crate::model::DeclSpace::Type {
            return match self.canonical_type(cfg, scope, segments, p) {
                Owner::InRepo { fqn, .. } => Outcome::Resolved(node_id(JavaLang::DOMAIN, &fqn)),
                Owner::Outside(package) => Outcome::External(package),
                Owner::Failed(reason) => Outcome::Unresolved(reason),
            };
        }
        // I-04 / §7.5.3: a single-static import names a *member name* on an
        // owner type — every overload of it, and possibly a field and a
        // member type as well. One declaration, one name, several targets, so
        // the whole group is what decides: one member is one edge, several is
        // `AmbiguousOverload`'s second clause, and none is the same honest
        // miss any other member lookup reports. Probing only the bare name
        // would have been the *field* key, which no method can ever take.
        let Some((member, owner_segments)) = segments.split_last() else {
            return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition);
        };
        let owner = self.canonical_type(cfg, scope, owner_segments, p);
        match &owner {
            Owner::Outside(package) => return Outcome::External(package.clone()),
            Owner::Failed(reason) => return Outcome::Unresolved(reason.clone()),
            Owner::InRepo { .. } => {}
        }
        let group = self.name_members(cfg, scope, &owner, member, p);
        self.select_name_group(group)
    }
}

/// Everything one file's header states about its scope.
fn build_scope(cfg: &JavaConfig, file: &FileFacts<JavaLang>) -> JavaScope {
    let header = &file.header;
    let mut scope = JavaScope {
        container: fqn::container(
            header.package.as_deref().unwrap_or(""),
            header.module.as_deref(),
        ),
        bindings: header.bindings.clone(),
        erased: header.erased.clone(),
        ..JavaScope::default()
    };
    let _ = cfg;
    for import in &header.imports {
        use crate::track_java::extract::ImportKind;
        match import.kind {
            ImportKind::SingleType => {
                if let Some(simple) = import.segments.last() {
                    scope
                        .single_type
                        .insert(simple.clone(), import.segments.clone());
                }
            }
            ImportKind::SingleStatic => {
                if let Some((member, owner)) = import.segments.split_last() {
                    scope.single_static.push((member.clone(), owner.to_vec()));
                }
            }
            ImportKind::TypeOnDemand => scope.type_on_demand.push(import.segments.clone()),
            ImportKind::StaticOnDemand => scope.static_on_demand.push(import.segments.clone()),
            ImportKind::Module => scope.module_imports.push(import.segments.clone()),
        }
    }
    for def in &file.defs {
        if def.kind != DefKind::Type {
            continue;
        }
        let mut path = def.owner.clone();
        path.push(def.name.clone());
        scope
            .file_types
            .entry(def.name.clone())
            .or_default()
            .push(path);
    }
    for decl in &header.types {
        scope
            .supers
            .insert(decl.path.join(&fqn::NEST.to_string()), decl.clone());
    }
    scope
}

impl JavaResolver {
    /// Resolve with exactly the argument facts carried by `r`.
    ///
    /// Ordinary resolution passes an untyped clone. The key-refinement hook
    /// calls this a second time only after that legacy pass reports overload
    /// ambiguity.
    fn resolve_dispatch(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let mut p = Probes {
            table: probe,
            seen: Vec::new(),
        };
        if r.locally_bound {
            return Resolution {
                outcome: Outcome::Unresolved(UnresolvedReason::LocalBinding),
                candidates: Vec::new(),
            };
        }
        let outcome = match r.kind {
            RefKind::Import => self.resolve_import(cfg, scope, r, &mut p),
            RefKind::Export => {
                let dotted = r.target.segments.join(".");
                match p.get(&dotted) {
                    Some(Entry::Container) => Outcome::Resolved(node_id(JavaLang::DOMAIN, &dotted)),
                    _ => Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
                }
            }
            RefKind::TypeUse | RefKind::Inherit | RefKind::Annotation => {
                self.resolve_type_ref(cfg, scope, r, &mut p)
            }
            RefKind::Call | RefKind::New | RefKind::FieldAccess | RefKind::MethodRef => {
                self.resolve_member_ref(cfg, scope, r, &mut p)
            }
            RefKind::Rebind => Outcome::Unresolved(UnresolvedReason::DynamicDispatch),
        };
        Resolution {
            outcome,
            candidates: p.seen,
        }
    }
}

impl Resolver<JavaLang> for JavaResolver {
    /// Phase 0 never fails for Java. B-04: there may be no parseable
    /// manifest at all, so requiring one would make every Gradle project a
    /// `ProjectLayoutUnknown` scan rather than a measured one.
    fn config(&self, _root: &Path, _files: &FileIndex) -> Result<JavaConfig, LayoutError> {
        Ok(JavaConfig::default())
    }

    /// Empty: Java reads no manifest, so no manifest fact can invalidate the
    /// graph. Both fields of [`JavaConfig`] are taught by the driver from the
    /// store as the scan proceeds — folding them in would change the
    /// fingerprint on every scan and wipe the graph each time.
    fn config_digest(&self, _cfg: &JavaConfig) -> Vec<u8> {
        Vec::new()
    }

    fn graph_revision(&self) -> u64 {
        1
    }

    /// P-01: the container a file decides the name of is the one it
    /// *declares*, never the one its directory suggests.
    fn declared_container(
        &self,
        _cfg: &JavaConfig,
        header: &JavaHeader,
    ) -> Option<(String, String)> {
        if let Some(module) = &header.module {
            return Some((fqn::container("", Some(module)), module.clone()));
        }
        let package = header.package.as_deref()?;
        if package.is_empty() {
            return None; // P-03: the unnamed package declares no name.
        }
        Some((package.to_string(), package.to_string()))
    }

    fn learn_containers(&self, cfg: &mut JavaConfig, names: &HashMap<String, String>) {
        for (container, name) in names {
            match container.strip_prefix("module:") {
                Some(_) => {
                    cfg.modules.insert(name.clone());
                }
                None => {
                    cfg.packages.insert(name.clone());
                }
            }
        }
    }

    /// P-07 and G-01. Note that Go's "a nested manifest means not ours" rule
    /// *inverts* here: a nested `pom.xml` or `build.gradle` is still this
    /// repository, because a Maven reactor is one graph.
    fn owns_file(&self, _cfg: &JavaConfig, rel_path: &str) -> bool {
        !Self::is_build_output(rel_path)
    }

    fn def_fqn(
        &self,
        _cfg: &JavaConfig,
        header: &JavaHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        let container = fqn::container(
            header.package.as_deref().unwrap_or(""),
            header.module.as_deref(),
        );
        match def.kind {
            DefKind::Module => Some(Fqn::new(container)),
            DefKind::Type => {
                let mut path = owner.to_vec();
                path.push(def.name.clone());
                Some(Fqn::new(fqn::type_fqn(&container, &path)))
            }
            DefKind::Method | DefKind::Constructor => {
                let callable = fqn::callable_of(def);
                let group =
                    fqn::overload_group(owner, &callable.name, callable.count(), callable.varargs);
                // M-01: when two declarations compete for the arity key,
                // neither takes it — both fall back to the signature form and
                // the key is the overload set's own node.
                let key = if header.overloaded.contains(&group) {
                    callable.signature()
                } else {
                    callable.key()
                };
                Some(Fqn::new(fqn::member_fqn(
                    &fqn::type_fqn(&container, owner),
                    &key,
                )))
            }
            // A field, an enum constant, or the overload-set marker, whose
            // name already *is* its key.
            _ => Some(Fqn::new(fqn::member_fqn(
                &fqn::type_fqn(&container, owner),
                &def.name,
            ))),
        }
    }

    /// A synthetic full-signature identity for a unique callable forwards to
    /// its established arity identity. Overload-set aliases carry an arity
    /// name and intentionally forward nowhere.
    fn def_alias_targets(
        &self,
        _cfg: &JavaConfig,
        header: &JavaHeader,
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Vec<Fqn> {
        if def.kind != DefKind::Alias || !def.name.contains('(') {
            return Vec::new();
        }
        let owner = fqn::type_fqn(
            &fqn::container(
                header.package.as_deref().unwrap_or(""),
                header.module.as_deref(),
            ),
            &def.owner,
        );
        if let Some(name) = fqn::varargs_prefix_name(&def.name) {
            return def.params.as_ref().map_or_else(Vec::new, |params| {
                vec![Fqn::new(fqn::member_fqn(
                    &owner,
                    &fqn::signature_key(name, &params.types),
                ))]
            });
        }
        let callable = fqn::callable_of(def);
        vec![Fqn::new(fqn::member_fqn(&owner, &callable.key()))]
    }

    /// Empty: the driver never calls this, and Java's arity key is part of
    /// the FQN rather than a second index beside it. See the module docs.
    fn index_keys(&self, _cfg: &JavaConfig, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        Vec::new()
    }

    /// Two Java declarations sharing an FQN are two entities (P-06): Maven
    /// reactor modules, Gradle source sets, product flavors and multi-release
    /// jars all produce same-named types that never co-compile, and merging
    /// them would let one definition's sites stand in for another's.
    fn mergeable(&self, _a: &Definition, _b: &Definition) -> bool {
        false
    }

    fn scope(
        &self,
        cfg: &JavaConfig,
        file: &FileFacts<JavaLang>,
        _probe: &dyn SymbolProbe,
    ) -> JavaScope {
        build_scope(cfg, file)
    }

    /// H-01: `extends` and `implements`. The driver resolves these before any
    /// member reference, and [`JavaResolver::lookup`] walks the relation it
    /// leaves — so a member declared three files above the receiver's type is
    /// reachable, where before it was one probe and a floor.
    ///
    /// One kind and not two: a `permits` clause names *subtypes*, and the
    /// extractor already emits those as plain type uses (see
    /// `supertype_heads`). Were they `Inherit`, this phase would walk member
    /// lookup *down* the hierarchy into declarations the receiver's type does
    /// not have.
    fn link_kinds(&self) -> &'static [RefKind] {
        &[RefKind::Inherit]
    }

    fn resolve(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let mut legacy = r.clone();
        legacy.arg_types = None;
        self.resolve_dispatch(cfg, scope, &legacy, probe)
    }

    fn resolve_with_key_refinement(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> (Resolution, RefKeyRefinement) {
        let legacy = self.resolve(cfg, scope, r, probe);
        if !matches!(
            legacy.outcome,
            Outcome::Unresolved(UnresolvedReason::AmbiguousOverload)
        ) {
            return (legacy, RefKeyRefinement::None);
        }
        let Some(arguments) = r.arg_types.clone() else {
            return (legacy, RefKeyRefinement::None);
        };
        let typed = self.resolve_dispatch(cfg, scope, r, probe);
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        for candidate in legacy
            .candidates
            .into_iter()
            .chain(typed.candidates.into_iter())
        {
            if seen.insert(candidate) {
                candidates.push(candidate);
            }
        }
        let outcome = match typed.outcome {
            Outcome::Resolved(id) => Outcome::Resolved(id),
            Outcome::Unresolved(UnresolvedReason::AmbiguousOverload) => {
                Outcome::Unresolved(UnresolvedReason::AmbiguousOverload)
            }
            _ => Outcome::Unresolved(UnresolvedReason::AmbiguousOverload),
        };
        (
            Resolution {
                outcome,
                candidates,
            },
            RefKeyRefinement::ArgumentTypes(arguments),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::JavaResolver;

    #[test]
    fn array_suffix_guard_recognizes_varargs_and_repeated_dimensions() {
        let spelling = |name: &str| vec![name.to_string()];

        assert!(JavaResolver::has_array_suffix(&spelling("String...")));
        assert!(JavaResolver::has_array_suffix(&spelling("T[][]")));
        assert!(!JavaResolver::has_array_suffix(&spelling("String")));
        assert!(!JavaResolver::has_array_suffix(&spelling(
            "java.lang.String"
        )));
    }
}
