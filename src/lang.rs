//! The extractor/resolver trait boundary the shared driver is generic over.
//!
//! The rule this module encodes: **shared code is generic over a
//! [`Language`], and every per-language type is an associated type the
//! shared code moves and never inspects.** `pipeline.rs` therefore names no
//! language's manifest, scope, or naming convention.
//!
//! It also makes the project's first non-negotiable a *type-level*
//! guarantee rather than a convention: [`Extractor::extract`] receives one
//! path and one source string, so an extractor has nothing it could link
//! against even if it wanted to. All linking happens in [`Resolver::resolve`],
//! which is the only place an [`Outcome`] is produced.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::Outcome;
use crate::model::{DefFacets, DefKind, Definition, Domain, Fqn, Lang, NodeId, RefKind, Reference};

/// One language's contribution to the shared driver: the constants it is
/// reported under and the three types only its own layers may read.
pub trait Language: Sized + 'static {
    /// The language records are attributed to in the report.
    const LANG: Lang;
    /// The identity space this language's nodes are hashed in.
    const DOMAIN: Domain;

    /// File extensions this language owns, without the dot: `["go"]`.
    fn extensions() -> &'static [&'static str];

    /// Directory names a scan never descends into.
    fn skip_dirs() -> &'static [&'static str] {
        &[]
    }

    /// Per-file facts the extractor produces and only the resolver reads.
    type Header;

    /// The resolver's per-file scope. The core never inspects it.
    type Scope;

    /// Project-level configuration, built once per scan.
    type Config;
}

/// Everything extracted from one file.
pub struct FileFacts<L: Language> {
    /// Language-private facts about the file itself.
    pub header: L::Header,
    /// Declarations the file makes.
    pub defs: Vec<Definition>,
    /// Sites in the file that name something possibly defined elsewhere.
    pub refs: Vec<Reference>,
}

impl<L: Language> Default for FileFacts<L>
where
    L::Header: Default,
{
    fn default() -> Self {
        FileFacts {
            header: L::Header::default(),
            defs: Vec::new(),
            refs: Vec::new(),
        }
    }
}

/// One file in, records out. Forbidden from linking.
pub trait Extractor<L: Language>: Send + Sync {
    /// Extract one file, in isolation. The signature is the enforcement:
    /// no probe, no config, no other file.
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<L>;
}

/// The repo-relative paths a scan's walk found, sorted.
pub struct FileIndex {
    /// Repo-relative, `/`-separated paths.
    pub files: Vec<String>,
}

/// A typed phase-0 failure: the project's layout could not be determined.
///
/// In the long run this is a per-file reason rather than a scan abort;
/// today the driver surfaces it as an `Err` from the scan, which is what it
/// already did, only typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutError {
    /// What could not be determined.
    pub message: String,
}

/// One classified reference plus every node identity read on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The single outcome. There is no way to express "dropped".
    pub outcome: Outcome<NodeId, String>,
    /// Every node identity this resolution read, hits and misses, in read
    /// order. Feeds the candidate-set invalidation index, so it must list
    /// exactly what was probed and nothing else.
    pub candidates: Vec<NodeId>,
}

/// What the symbol table holds under one node identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A definition, with the facts a resolver may branch on.
    Definition {
        /// What a reference can do with it.
        kind: DefKind,
        /// Attributes the owning resolver reads.
        facets: DefFacets,
    },
    /// A package or module.
    Container,
    /// A dependency outside this repository.
    External,
    /// An alias: the identity forwards to exactly one other.
    ///
    /// A re-export, an export rename, a module-level import binding. The
    /// alias is still a node — a reference really does name it, and the
    /// barrel's own outgoing edge starts there — so a resolver that cannot
    /// follow the forward may answer with the alias itself and still be
    /// telling the truth.
    Alias {
        /// What it forwards to.
        target: NodeId,
    },
    /// An index key standing for several identities: an overload set, or the
    /// modules a star export forwards.
    ///
    /// The members are not all definitions. `export * from './a'` puts a
    /// *module* here, because the names it supplies are a fact about that
    /// module rather than about the key — the resolver re-enters it and looks
    /// the name up there.
    Set(Vec<NodeId>),
}

/// What one type declares as its direct supertypes, as the supertype phase
/// placed them.
///
/// Direct and not transitive on purpose. A resolver that walks the relation
/// one hop at a time reads the identity of every type on the way, and those
/// reads land in the candidate index — which is what makes an edit to a base
/// class three levels up wake the member reference that depended on it. A
/// pre-computed closure would answer in one read and leave the intermediate
/// types unrecorded, so an incremental scan would stop matching a cold one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supertypes {
    /// The supertypes that resolved to a definition in this repository, in
    /// the order the type declared them.
    pub fqns: Vec<Fqn>,
    /// Whether every supertype the type declares is in [`Supertypes::fqns`].
    ///
    /// `false` when one was external, unresolved, or not a definition. The
    /// closure below this type is then short, and a resolver that reports a
    /// miss under it must say so rather than claim the name is absent.
    pub complete: bool,
}

/// The resolver's view of the symbol table: one lookup per candidate.
pub trait SymbolProbe {
    /// What the graph holds under this identity, if anything.
    fn probe(&self, id: &NodeId) -> Option<Entry>;

    /// The direct supertypes of a type identity.
    ///
    /// `None` when this scan holds no supertype fact for the identity: it is
    /// not a type, or the language declares no [`Resolver::link_kinds`] and
    /// the driver therefore ran no supertype phase over it. A type that
    /// declares no supertype at all answers `Some` with an empty, complete
    /// list — "nothing above it" and "nothing known about it" are different
    /// facts, and a resolver that confuses them either invents a complete
    /// closure or refuses to believe one.
    ///
    /// Defaulted so a table that carries no such fact — the plain symbol map
    /// a unit test hands a resolver — is still a [`SymbolProbe`].
    fn supertypes(&self, _id: &NodeId) -> Option<Supertypes> {
        None
    }
}

/// A membership-only probe: a set knows presence, not kind.
///
/// [`Entry::Container`] is the honest answer for a set — it asserts the
/// identity exists and nothing more. The Go resolver reads only presence,
/// so this is sufficient today; a typed store replaces it rather than
/// extending it.
impl SymbolProbe for HashSet<NodeId> {
    fn probe(&self, id: &NodeId) -> Option<Entry> {
        self.contains(id).then_some(Entry::Container)
    }
}

/// All of one language's linking decisions. Never drops.
pub trait Resolver<L: Language>: Send + Sync {
    /// Phase 0: work out the project's layout. Manifest parsing is
    /// resolver-internal; the core only moves the result.
    fn config(&self, root: &Path, files: &FileIndex) -> Result<L::Config, LayoutError>;

    /// A fingerprint of everything the project's *manifest* decides.
    ///
    /// The manifest is a scan input the walk never hashes: it carries no
    /// extension the language owns and contributes no facts of its own. It
    /// still decides every identity in the graph — a module path is the root
    /// of every FQN beneath it — so a store built under a different one
    /// describes a different project and cannot be patched into this one
    /// file by file.
    ///
    /// Covers only what phase 0 read. Anything the driver teaches the config
    /// afterwards — see [`Resolver::learn_containers`] — changes as the scan
    /// learns rather than as the project does, and folding it in here would
    /// wipe the store on every scan.
    ///
    /// A language with no project manifest returns an empty fingerprint and
    /// is never invalidated by this.
    fn config_digest(&self, cfg: &L::Config) -> Vec<u8>;

    /// The container this file *decides the name of*, as
    /// `(container identity, declared name)`.
    ///
    /// Both phases build identities by asking what a container is called, so
    /// they have to ask with the same knowledge. The store answers for files
    /// an event did not touch; this answers for the ones it did, and the
    /// driver folds the result in *before* the definition phase. Without it
    /// phase 1 sees only what earlier scans stored while phase 2 sees what
    /// phase 1 just wrote, and one file's definitions can land under one
    /// identity with their edges sourced at another.
    ///
    /// `None` when the file does not decide that name. A Go `_test.go` file
    /// may declare an external test package — `package foo_test` beside
    /// package `foo` — which is a container of its own rather than a
    /// statement about the directory's.
    fn declared_container(&self, cfg: &L::Config, header: &L::Header) -> Option<(String, String)>;

    /// Fold container names the store already holds into the config.
    ///
    /// Binding an unaliased import needs a fact out of the *imported*
    /// container's source, so it is not per-file derivable; the driver is
    /// the only layer that sees every container, and this is how it hands
    /// them over without inspecting [`Language::Config`]. A language whose
    /// bindings are per-file derivable ignores the call.
    fn learn_containers(&self, cfg: &mut L::Config, names: &HashMap<String, String>);

    /// Whether this file belongs to the scan at all. Go excludes files
    /// governed by a nested manifest; the core never learns why.
    fn owns_file(&self, cfg: &L::Config, rel_path: &str) -> bool;

    /// Canonical FQN for a definition, or `None` when it is not nameable —
    /// the caller then emits no node, and references inside it source at the
    /// file's container.
    ///
    /// Takes the probe because building an FQN is itself a resolution step
    /// in some languages and a pure function in others.
    fn def_fqn(
        &self,
        cfg: &L::Config,
        header: &L::Header,
        owner: &[String],
        def: &Definition,
        probe: &dyn SymbolProbe,
    ) -> Option<Fqn>;

    /// What this definition forwards to, when it is an alias.
    ///
    /// Runs in the definition phase, beside [`Resolver::def_fqn`] and with
    /// the same inputs, because an alias's target is part of what the
    /// identity *means* and the symbol table has to carry it before any
    /// reference is resolved against it. That is also why the answer is an
    /// [`Fqn`] and not an edge: the extractor emitted only raw text — a
    /// specifier and a name — and turning raw text into an identity is the
    /// resolver's job in either phase.
    ///
    /// Empty for every ordinary definition, and empty too for an alias key
    /// that stands for a set without forwarding to it.
    fn def_alias_targets(
        &self,
        _cfg: &L::Config,
        _header: &L::Header,
        _def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Vec<Fqn> {
        Vec::new()
    }

    /// Extra keys in the [`NodeId`] keyspace this definition must be
    /// reachable under. Empty when a definition is reachable only by its FQN.
    fn index_keys(&self, cfg: &L::Config, fqn: &Fqn, def: &Definition) -> Vec<NodeId>;

    /// Two definitions share an FQN: language semantics, or corruption?
    fn mergeable(&self, a: &Definition, b: &Definition) -> bool;

    /// Build the per-file scope. Runs after the definition phase and sees
    /// the probe, because import binding is not per-file derivable.
    fn scope(&self, cfg: &L::Config, file: &FileFacts<L>, probe: &dyn SymbolProbe) -> L::Scope;

    /// Reference kinds the driver resolves *before* ordinary resolution, to
    /// build the supertype relation every member lookup then reads. Empty
    /// when the language has none.
    ///
    /// The driver runs this phase between the definition phase and the
    /// reference phase, once, against definitions alone. That is deliberate
    /// and not a shortcut: a base-class name is placed by the definition
    /// table, so the relation cannot depend on itself and no fixed point is
    /// needed. Where a base name *would* need the closure to be placed, the
    /// phase misses it and marks the type's [`Supertypes::complete`] false —
    /// an under-approximation that says so, which is the honest shape for a
    /// bound.
    ///
    /// Only a reference whose nearest nameable encloser is the *subtype*
    /// contributes: the driver files the resolved target under the same
    /// identity [`Resolver::def_fqn`] gave that encloser, so the relation and
    /// the node it hangs on cannot drift apart.
    fn link_kinds(&self) -> &'static [RefKind];

    /// The only place an [`Outcome`] is produced. Never drops.
    fn resolve(
        &self,
        cfg: &L::Config,
        scope: &L::Scope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Domain, node_id};

    #[test]
    fn a_set_probe_answers_presence_and_nothing_more() {
        let known = node_id(Domain::Go, "m/pkg#Foo");
        let unknown = node_id(Domain::Go, "m/pkg#Bar");
        let mut table: HashSet<NodeId> = HashSet::new();
        table.insert(known);
        assert_eq!(table.probe(&known), Some(Entry::Container));
        assert_eq!(table.probe(&unknown), None);
    }
}
