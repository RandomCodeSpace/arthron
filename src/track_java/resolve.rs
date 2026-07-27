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
//!   a [`DefKind::Alias`] node the extractor emits at the shared key. A probe
//!   that lands on the alias reports `AmbiguousOverload`; one that misses has
//!   found "not declared on this type" and may walk on. See
//!   [`crate::track_java::fqn`].
//! * **No supertype-closure phase (H-01).** `link_kinds` is likewise never
//!   driven, and nothing in the store enumerates a type's supertypes — the
//!   probe answers one identity at a time. So the closure is walked *only for
//!   types the file being resolved declares*, where `extends`/`implements` are
//!   a single-file fact ([`TypeDecl`]). Everything beyond that is
//!   [`UnresolvedReason::UnindexedSupertype`], which is B-05's own reason and
//!   is expected to be a large, honest floor rather than a rate to be gamed
//!   down.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::lang::{
    Entry, FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe,
};
use crate::model::{DefKind, Definition, Fqn, NodeId, RefKind, Reference, TargetRoot, node_id};
use crate::track_java::JavaLang;
use crate::track_java::extract::{Binding, BindingKind, JavaHeader, TypeDecl};
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
        let qualifier = segments[..segments.len() - 1].join(".");
        Owner::Outside(outside(&qualifier))
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
        site: u32,
        enclosing: &[String],
        p: &mut Probes<'_>,
    ) -> (Owner, usize) {
        if segments.is_empty() {
            return (Owner::Failed(UnresolvedReason::NoMatchingDefinition), 0);
        }
        // §6.4.2: a variable obscures a type, which obscures a package. X-02:
        // the declared type is written in this file, so this is a lookup and
        // not inference.
        if let Some(binding) = scope.binding_at(&segments[0], site) {
            let Some(declared) = binding.declared_type.clone() else {
                // A lambda parameter, an unreadable `var` (X-03), an array or
                // a primitive: the receiver is a name with no stated type.
                return (Owner::Failed(UnresolvedReason::NeedsTypeInference), 1);
            };
            let owner = self.canonical_type(cfg, scope, &declared, p);
            return (owner, 1);
        }
        if let Some(head) = self.simple_type(cfg, scope, &segments[0], enclosing, p) {
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
        // §6.5.2's reclassification needs the symbol table and the table has
        // no opinion. Naming this `NeedsTypeInference` would hide a case that
        // needs no inference at all behind an honest-sounding label.
        (
            Owner::Failed(UnresolvedReason::AmbiguousName),
            segments.len(),
        )
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
    fn lookup(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        owner: &Owner,
        keys: &[String],
        p: &mut Probes<'_>,
    ) -> Member {
        let (fqn_start, local_start) = match owner {
            Owner::InRepo { fqn, local } => (fqn.clone(), local.clone()),
            _ => {
                return Member::Missing { unindexed: true };
            }
        };
        let mut unindexed = false;
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: Vec<(String, Option<Vec<String>>)> = vec![(fqn_start, local_start)];
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
                    Some(Entry::Definition { .. }) => {
                        let id = node_id(JavaLang::DOMAIN, &fqn::member_fqn(&type_fqn, key));
                        return Member::Found(id);
                    }
                    _ => {}
                }
            }
            // Only a type this file declares states its own supertypes. For
            // anything else the closure is unreachable — the probe answers
            // one identity at a time and nothing enumerates.
            let Some(path) = local else {
                unindexed = true;
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
        Member::Missing { unindexed }
    }

    /// Turn a member lookup into an outcome, with the honest miss.
    fn select(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        owner: Owner,
        name: &str,
        argc: Option<u32>,
        p: &mut Probes<'_>,
    ) -> Outcome<NodeId, String> {
        match owner {
            Owner::Failed(reason) => Outcome::Unresolved(reason),
            Owner::Outside(package) => Outcome::External(package),
            owner @ Owner::InRepo { .. } => {
                let keys = Self::member_keys(name, argc);
                match self.lookup(cfg, scope, &owner, &keys, p) {
                    Member::Found(id) => Outcome::Resolved(id),
                    Member::Ambiguous => Outcome::Unresolved(UnresolvedReason::AmbiguousOverload),
                    Member::Missing { unindexed } => {
                        if is_object_member(name, argc) {
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
        let site = r.span.byte_start;
        let segments = &r.target.segments;

        // C-03: `this(…)` and `super(…)` name a constructor exactly, and
        // C-01's creation site names one on the type it wrote.
        if r.kind == RefKind::New {
            return self.resolve_new(cfg, scope, r, &enclosing, p);
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
                let path = self.this_path(scope, outer, &enclosing);
                match self.enclosing_owner(scope, &path, p) {
                    Some(owner) => owner,
                    None => return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
                }
            }
            TargetRoot::Super { qualifier: outer } => {
                match self.super_owner(cfg, scope, outer, &enclosing, p) {
                    Some(owner) => owner,
                    None => return Outcome::External(JAVA_LANG_PACKAGE.to_string()),
                }
            }
            TargetRoot::Name if qualifier.is_empty() => {
                // N-02: an unqualified invocation. §6.5.1 makes a bare
                // `m(…)` a MethodName and nothing else, so no local can
                // shadow it and the search is purely the enclosing chain.
                return self.unqualified(cfg, scope, name, argc, &enclosing, p);
            }
            TargetRoot::Name => {
                let (owner, consumed) = self.qualifier(cfg, scope, qualifier, site, &enclosing, p);
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
        // A `this.a.m()` or `super.a.m()` chain selects a member first, whose
        // type this resolver does not compute.
        if !matches!(r.target.root, TargetRoot::Name) && qualifier.len() > 1 {
            return Outcome::Unresolved(UnresolvedReason::NeedsTypeInference);
        }
        if !matches!(r.target.root, TargetRoot::Name) && qualifier.len() == 1 {
            return Outcome::Unresolved(UnresolvedReason::NeedsTypeInference);
        }
        if r.kind == RefKind::MethodRef {
            return match owner {
                Owner::Failed(reason) => Outcome::Unresolved(reason),
                Owner::Outside(package) => Outcome::External(package),
                Owner::InRepo { .. } => Outcome::Unresolved(UnresolvedReason::AmbiguousOverload),
            };
        }
        self.select(cfg, scope, owner, name, argc, p)
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
    /// `None` when the enclosing class declares no `extends` clause: its
    /// superclass is `java.lang.Object` (§8.1.4), which is never indexed.
    fn super_owner(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        outer: &[String],
        enclosing: &[String],
        p: &mut Probes<'_>,
    ) -> Option<Owner> {
        if !outer.is_empty() {
            // `Iface.super.m()` targets exactly `Iface#m` — no ambiguity and
            // no inference.
            return Some(self.canonical_type(cfg, scope, outer, p));
        }
        let decl = scope.supers.get(&enclosing.join(&fqn::NEST.to_string()))?;
        let segments = decl.superclass.clone()?;
        Some(self.canonical_type(cfg, scope, &segments, p))
    }

    /// N-02's ordered candidate list for an unqualified invocation.
    fn unqualified(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        name: &str,
        argc: Option<u32>,
        enclosing: &[String],
        p: &mut Probes<'_>,
    ) -> Outcome<NodeId, String> {
        let keys = Self::member_keys(name, argc);
        let mut unindexed = enclosing.is_empty();
        // 1 and 2: the innermost enclosing type and its supertype closure,
        // then each lexically enclosing type outward (§15.12.1).
        for depth in (1..=enclosing.len()).rev() {
            let path = &enclosing[..depth];
            let Some(owner) = self.enclosing_owner(scope, path, p) else {
                unindexed = true;
                continue;
            };
            match self.lookup(cfg, scope, &owner, &keys, p) {
                Member::Found(id) => return Outcome::Resolved(id),
                Member::Ambiguous => {
                    return Outcome::Unresolved(UnresolvedReason::AmbiguousOverload);
                }
                Member::Missing { unindexed: more } => unindexed |= more,
            }
        }
        // 3 and 4: single-static-import owners, then static-import-on-demand
        // owners (§7.5.3, §7.5.4).
        let statics = scope
            .single_static
            .iter()
            .filter(|(member, _)| member == name)
            .map(|(_, owner)| owner.clone())
            .chain(scope.static_on_demand.iter().cloned());
        for segments in statics {
            let owner = self.canonical_type(cfg, scope, &segments, p);
            match &owner {
                Owner::Outside(package) => return Outcome::External(package.clone()),
                Owner::Failed(_) => {
                    unindexed = true;
                    continue;
                }
                Owner::InRepo { .. } => {}
            }
            match self.lookup(cfg, scope, &owner, &keys, p) {
                Member::Found(id) => return Outcome::Resolved(id),
                Member::Ambiguous => {
                    return Outcome::Unresolved(UnresolvedReason::AmbiguousOverload);
                }
                Member::Missing { unindexed: more } => unindexed |= more,
            }
        }
        if is_object_member(name, argc) {
            return Outcome::External(JAVA_LANG_PACKAGE.to_string());
        }
        if unindexed {
            Outcome::Unresolved(UnresolvedReason::UnindexedSupertype)
        } else {
            Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition)
        }
    }

    /// C-01, C-03, C-04: an object creation site names a constructor.
    fn resolve_new(
        &self,
        cfg: &JavaConfig,
        scope: &JavaScope,
        r: &Reference,
        enclosing: &[String],
        p: &mut Probes<'_>,
    ) -> Outcome<NodeId, String> {
        let owner = match &r.target.root {
            TargetRoot::This { .. } => match self.enclosing_owner(scope, enclosing, p) {
                Some(owner) => owner,
                None => return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition),
            },
            TargetRoot::Super { .. } => {
                // §8.8.7: `super(…)` names the superclass's constructor, and
                // a class with no `extends` clause extends `java.lang.Object`.
                match self.super_owner(cfg, scope, &[], enclosing, p) {
                    Some(owner) => owner,
                    None => return Outcome::External(JAVA_LANG_PACKAGE.to_string()),
                }
            }
            TargetRoot::Expr => return Outcome::Unresolved(UnresolvedReason::NeedsExpressionType),
            TargetRoot::Name => {
                if r.target.segments.len() == 1 {
                    self.simple_type(cfg, scope, &r.target.segments[0], enclosing, p)
                        .unwrap_or_else(|| Owner::Failed(self.simple_type_miss(cfg, scope)))
                } else {
                    self.canonical_type(cfg, scope, &r.target.segments, p)
                }
            }
        };
        self.select(cfg, scope, owner, fqn::INIT, r.argc, p)
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
        // member type as well. One declaration, one name, several targets:
        // the field is probed because it is the only one an arity-free key
        // can name, and an overload set with no arity to discriminate on is
        // exactly `AmbiguousOverload`'s second clause.
        let Some((member, owner_segments)) = segments.split_last() else {
            return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition);
        };
        let owner = self.canonical_type(cfg, scope, owner_segments, p);
        match &owner {
            Owner::Outside(package) => return Outcome::External(package.clone()),
            Owner::Failed(reason) => return Outcome::Unresolved(reason.clone()),
            Owner::InRepo { .. } => {}
        }
        match self.lookup(
            cfg,
            scope,
            &owner,
            std::slice::from_ref(&member.to_string()),
            p,
        ) {
            Member::Found(id) => Outcome::Resolved(id),
            Member::Ambiguous => Outcome::Unresolved(UnresolvedReason::AmbiguousOverload),
            Member::Missing { .. } => Outcome::Unresolved(UnresolvedReason::AmbiguousOverload),
        }
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

    /// Empty, and not because Java has nothing to drive to a fixed point:
    /// H-01's supertype closure is exactly that, and the driver never calls
    /// this. The in-file closure in [`JavaResolver::lookup`] is what stands
    /// in, and `UnindexedSupertype` is what the rest honestly costs.
    fn link_kinds(&self) -> &'static [RefKind] {
        &[]
    }

    fn resolve(
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
        // A reference whose *target* is the bound name itself: a type
        // parameter (§4.4), a local class (§14.3), or the constructor of one.
        // None is a node, so linking it would be a wrong edge.
        //
        // A receiver that happens to be a local is deliberately *not* here:
        // `f.m()` names `m`, which is a node, and X-02 is the whole reason the
        // extractor states `f`'s declared type. Reading `locally_bound` the
        // way Go does would delete most Java calls from both terms of the
        // resolution rate and raise the number without linking anything.
        if r.locally_bound
            && r.target.segments.len() == 1
            && matches!(
                r.kind,
                RefKind::TypeUse | RefKind::Inherit | RefKind::Annotation | RefKind::New
            )
        {
            return Resolution {
                outcome: Outcome::Unresolved(UnresolvedReason::LocalBinding),
                candidates: Vec::new(),
            };
        }
        let outcome = match r.kind {
            RefKind::Import => self.resolve_import(cfg, scope, r, &mut p),
            RefKind::Export => {
                // §7.7.2: a module exports one of its own packages.
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
            // The Java extractor emits none: nothing in Java rebinds a name
            // at a site the way a monkeypatch does.
            RefKind::Rebind => Outcome::Unresolved(UnresolvedReason::DynamicDispatch),
        };
        Resolution {
            outcome,
            candidates: p.seen,
        }
    }
}
