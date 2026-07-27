//! The one place a PHP [`crate::Outcome`] is produced. Never drops.
//!
//! # The gate is an import-resolution rate
//!
//! PHP is a **tier-2** language here: definitions, structure and imports, and
//! no verified call edges. So the references this resolver classifies are
//! exactly the `use` imports the extractor emits, and the rate the gate
//! defends is the share of them that name something this repository declares.
//! Every one ends `Resolved`, `External`, or `Unresolved` with a reason —
//! there is no way to express "dropped".
//!
//! # The FQN grammar
//!
//! ```text
//! container  := Ident ("\" Ident)*         GuzzleHttp\Cookie       ("" is the global namespace)
//! type       := container "#" Ident        GuzzleHttp\Cookie#CookieJar
//! function   := container "#" Ident "()"   GuzzleHttp\Handler#curl_setopt()
//! constant   := container "#" Ident "!"    GuzzleHttp#VERSION!
//! method     := type "::" Ident "()"       GuzzleHttp\Cookie#CookieJar::toArray()
//! property   := type "::$" Ident           GuzzleHttp\Cookie#CookieJar::$cookies
//! classconst := type "::" Ident "!"        GuzzleHttp#Client::MAJOR_VERSION!
//! ```
//!
//! Three invariants, and the third is PHP's own:
//!
//! 1. **`#` separates a container from its members, and a container's own
//!    name carries none** — the repository's convention, already true of Go's
//!    `{import path}#{Recv}.{name}` and Java's `{package}#{Type}`. It is what
//!    keeps the namespace `A\B` and the class `B` of namespace `A` two
//!    identities, where PHP spells both `A\B`.
//! 2. **`\` only joins namespace segments**, before the `#`, and `::` only
//!    steps from a type to one of its members.
//! 3. **A sigil says which of PHP's three symbol tables a name lives in.**
//!    `()` for a callable, `!` for a constant, nothing for a type, `$` for a
//!    property. PHP lets one namespace hold a class `Foo`, a function `Foo`
//!    and a constant `Foo` at once, and a class hold a method `X`, a constant
//!    `X` and a property `$X` at once; without the sigils each pair would
//!    hash to one node. `(`, `)`, `!` and `$` are all illegal in a PHP
//!    identifier, so no name can forge one.
//!
//! # The import model
//!
//! A `use` names an absolute name, so resolution is a probe rather than a
//! search — which is why [`PhpScope`] is empty. The rules, in order:
//!
//! 1. **Probe the key the clause's own keyword picks.** `use A\B;` probes the
//!    type `A#B`; `use function A\b;` probes `A#b()`; `use const A\B;` probes
//!    `A#B!`. A hit is [`crate::Outcome::Resolved`].
//! 2. **A plain `use` may name a namespace**, not a class — `use GuzzleHttp\Promise as P;`
//!    then `P\Utils::…`. So a class-form miss probes the container key next.
//! 3. **No PSR-4 prefix was read at all** ⇒
//!    [`crate::UnresolvedReason::ProjectLayoutUnknown`]. Without a map this
//!    build cannot say whether a name should be here, and blaming the name
//!    for that would be blaming the corpus for arthron's own blind spot.
//! 4. **A prefix this repository declares claims the name** ⇒ the name should
//!    be here and is not. If PSR-4 maps it onto a file the walk *did* find,
//!    the file is here and the name is absent, which is
//!    [`crate::UnresolvedReason::NoMatchingDefinition`] — the bucket reserved
//!    for meaning our own bug. Otherwise the map points at nothing, which is
//!    [`crate::UnresolvedReason::ModuleNotFound`].
//! 5. **No declared prefix claims it** ⇒ [`crate::Outcome::External`], named
//!    by its root namespace segment. This is the provenance's rule: a `use`
//!    naming an undeclared vendor namespace is external.
//!
//! # The honesty posture, and what it costs
//!
//! Rule 4 is where this track's floor is, and it is deliberate. A sibling
//! composer package that shares a vendor namespace root — `GuzzleHttp\Psr7`
//! and `GuzzleHttp\Promise` beside `GuzzleHttp` itself — falls under this
//! repository's own `GuzzleHttp\` → `src/` claim, so PSR-4 says the name
//! should be at `src/Psr7/Request.php` and it is not. That is
//! `ModuleNotFound`, it counts **against** the rate, and it is the largest
//! single bucket on the vendored corpus.
//!
//! Calling those `External` instead would take them out of both terms and
//! lift the rate to a perfect 1.0 without linking one extra reference. The
//! two facts that would actually close the gap are a `composer.lock` or an
//! installed `vendor/` tree, and neither is here — a package name does not
//! give its namespace (`guzzlehttp/promises` supplies `GuzzleHttp\Promise`),
//! so there is no derivation to write, only a guess to decline.
//!
//! # Known non-claims
//!
//! - **`use function` and `use const` are unexercised.** The vendored corpus
//!   contains none, and no `files` autoload entry either, so PHP's
//!   fallback-to-global rule for an unqualified function call is not
//!   measured here. The rules above are written for them; the corpus does not
//!   check them. That shape needs a second PHP corpus.
//! - **A trait `use` inside a class body is not resolved**, because it is not
//!   emitted: composing a trait is an inheritance fact, and tier 2 verifies
//!   no inheritance.
//! - **A fully-qualified name written inline** — `new \Psr\Http\Message\Uri()`
//!   — is a type use, which tier 2 does not emit and so does not resolve.

use std::collections::HashMap;
use std::path::Path;

use crate::lang::{
    Extractor, FileFacts, FileIndex, LayoutError, Resolution, Resolver, SymbolProbe,
};
use crate::model::{DefKind, Definition, Fqn, NodeId, RefKind, Reference, node_id};
use crate::track_php::extract::{PhpExtractor, PhpHeader, UseKind};
use crate::track_php::lang::PhpLang;
use crate::track_php::project::{self, PhpProject};
use crate::{Outcome, UnresolvedReason};

use crate::lang::Language;

/// PHP's per-file scope: nothing.
///
/// Not an oversight and not a placeholder. The only reference kind this track
/// emits is the `use` import itself, and a `use` names an absolute name, so
/// there is no file-local environment to read it against. The bindings a
/// `use` *creates* matter only to the expression-level references tier 2 does
/// not emit; the day PHP goes to tier 1, this is where they land.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhpScope;

/// The PHP resolver.
pub struct PhpResolver;

/// Separates a container from its members. Never appears in a namespace name.
const MEMBER: char = '#';

/// The container FQN of a namespace. The global namespace is the empty
/// string: it is a container with no name, which is a different fact from a
/// file naming no container.
fn container(namespace: &str) -> String {
    namespace.to_string()
}

fn type_fqn(namespace: &str, name: &str) -> String {
    format!("{namespace}{MEMBER}{name}")
}

fn function_fqn(namespace: &str, name: &str) -> String {
    format!("{namespace}{MEMBER}{name}()")
}

fn const_fqn(namespace: &str, name: &str) -> String {
    format!("{namespace}{MEMBER}{name}!")
}

fn method_fqn(namespace: &str, class: &str, name: &str) -> String {
    format!("{}::{name}()", type_fqn(namespace, class))
}

fn property_fqn(namespace: &str, class: &str, name: &str) -> String {
    format!("{}::${name}", type_fqn(namespace, class))
}

fn class_const_fqn(namespace: &str, class: &str, name: &str) -> String {
    format!("{}::{name}!", type_fqn(namespace, class))
}

/// The name an external node is filed under.
///
/// The root namespace segment, which is the coarsest unit PHP's own
/// resolution keys on and the only one this build can name without guessing
/// where a composer package's namespace begins. A one-segment name is in the
/// global namespace, which is the runtime's own — `\RuntimeException`,
/// `\Closure`, `\DateTime` — and they share one node rather than each minting
/// a package that does not exist.
fn external_package(segments: &[String]) -> String {
    match segments.len() {
        0 | 1 => "php:global".to_string(),
        _ => segments[0].clone(),
    }
}

/// Every probe a resolution made, in read order, hits and misses alike.
struct Probes<'a> {
    table: &'a dyn SymbolProbe,
    seen: Vec<NodeId>,
}

impl Probes<'_> {
    fn hit(&mut self, fqn: &str) -> bool {
        let id = node_id(PhpLang::DOMAIN, fqn);
        self.seen.push(id);
        self.table.probe(&id).is_some()
    }

    fn id(fqn: &str) -> NodeId {
        node_id(PhpLang::DOMAIN, fqn)
    }
}

impl PhpResolver {
    /// Rule 1 through rule 5, in order.
    fn resolve_import(cfg: &PhpProject, r: &Reference, p: &mut Probes) -> Outcome<NodeId, String> {
        let segments = &r.target.segments;
        let Some((last, prefix)) = segments.split_last() else {
            // An import with no name is not something the extractor emits: a
            // clause with no name yields no reference at all.
            return Outcome::Unresolved(UnresolvedReason::NoMatchingDefinition);
        };
        let namespace = prefix.join("\\");
        let kind = UseKind::of(&r.raw_target);
        let key = match kind {
            UseKind::Class => type_fqn(&namespace, last),
            UseKind::Function => function_fqn(&namespace, last),
            UseKind::Const => const_fqn(&namespace, last),
        };
        if p.hit(&key) {
            return Outcome::Resolved(Probes::id(&key));
        }
        // Rule 2: a plain `use` may name a namespace rather than a class.
        // Only the class form can: `use function` and `use const` name a
        // member of a namespace and never the namespace itself.
        if kind == UseKind::Class {
            let whole = container(&segments.join("\\"));
            if p.hit(&whole) {
                return Outcome::Resolved(Probes::id(&whole));
            }
        }
        if !cfg.layout_known() {
            // Rule 3.
            return Outcome::Unresolved(UnresolvedReason::ProjectLayoutUnknown);
        }
        match cfg.claiming_prefix(segments) {
            // Rule 4.
            Some((prefix_len, dirs)) => {
                let claimed_here =
                    kind == UseKind::Class && cfg.psr4_file_exists(dirs, &segments[prefix_len..]);
                Outcome::Unresolved(if claimed_here {
                    UnresolvedReason::NoMatchingDefinition
                } else {
                    // PSR-4 maps class names onto files and says nothing
                    // about functions or constants — `files` autoload is what
                    // loads those — so a `use function` miss under a claimed
                    // prefix has no path to test and takes this branch.
                    UnresolvedReason::ModuleNotFound
                })
            }
            // Rule 5.
            None => Outcome::External(external_package(segments)),
        }
    }
}

impl Resolver<PhpLang> for PhpResolver {
    /// Phase 0 never fails. A repository with no `composer.json`, or one
    /// whose manifest is unreadable, is still full of extractable PHP; what
    /// it costs is stated per reference, with a reason, rather than as a scan
    /// that refuses to run.
    fn config(&self, root: &Path, files: &FileIndex) -> Result<PhpProject, LayoutError> {
        Ok(project::load(root, &files.files))
    }

    /// The PSR-4 map, and only it. The file set is a phase-0 input too and is
    /// deliberately absent: it changes whenever a file is added, and folding
    /// it in here would wipe the store on every scan.
    fn config_digest(&self, cfg: &PhpProject) -> Vec<u8> {
        let mut out = Vec::new();
        for (prefix, dirs) in &cfg.psr4 {
            out.extend_from_slice(prefix.as_bytes());
            out.push(0);
            for dir in dirs {
                out.extend_from_slice(dir.as_bytes());
                out.push(1);
            }
            out.push(2);
        }
        out
    }

    /// `None`: a PHP identity is decided by the `namespace` the file itself
    /// declares, so both phases build the same names from the same bytes and
    /// there is nothing to learn from the store. A file may declare several
    /// namespaces anyway, and this asks for one.
    fn declared_container(
        &self,
        _cfg: &PhpProject,
        _header: &PhpHeader,
    ) -> Option<(String, String)> {
        None
    }

    /// Nothing to learn, for the reason [`Resolver::declared_container`]
    /// gives.
    fn learn_containers(&self, _cfg: &mut PhpProject, _names: &HashMap<String, String>) {}

    /// Every file the walk reached. Go's "a nested manifest means not ours"
    /// rule inverts here, the way it does for Java: a monorepo's nested
    /// `composer.json` is still this repository's code. What is genuinely not
    /// ours is `vendor/`, and that is pruned from the walk by
    /// [`PhpLang::skip_dirs`] rather than filtered out of it.
    fn owns_file(&self, _cfg: &PhpProject, _rel_path: &str) -> bool {
        true
    }

    fn def_fqn(
        &self,
        _cfg: &PhpProject,
        _header: &PhpHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        // A namespace names itself; everything else carries its namespace as
        // `owner[0]` and, for a member, its type as `owner[1]`. A file may
        // declare several namespaces, so the namespace cannot be a property
        // of the header.
        if def.kind == DefKind::Module {
            return Some(Fqn::new(container(&def.name)));
        }
        let namespace = owner.first()?.as_str();
        match (def.kind, owner.get(1)) {
            (DefKind::Type, None) => Some(Fqn::new(type_fqn(namespace, &def.name))),
            (DefKind::Function, None) => Some(Fqn::new(function_fqn(namespace, &def.name))),
            (DefKind::Const, None) => Some(Fqn::new(const_fqn(namespace, &def.name))),
            (DefKind::Method | DefKind::Constructor, Some(class)) => {
                Some(Fqn::new(method_fqn(namespace, class, &def.name)))
            }
            (DefKind::Field | DefKind::Property, Some(class)) => {
                Some(Fqn::new(property_fqn(namespace, class, &def.name)))
            }
            (DefKind::Const, Some(class)) => {
                Some(Fqn::new(class_const_fqn(namespace, class, &def.name)))
            }
            // A shape the extractor does not produce — a member with no type,
            // a type inside one. Not nameable, so not a node, rather than
            // named by a rule nobody wrote.
            _ => None,
        }
    }

    /// Empty: PHP reaches every definition by its FQN alone. There are no
    /// overload sets — one name is one declaration — and no export aliases:
    /// `use A\B as C;` binds `C` in one file and is nameable from nowhere
    /// else.
    fn index_keys(&self, _cfg: &PhpProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        Vec::new()
    }

    /// Two PHP declarations sharing an FQN are two entities. Redeclaring a
    /// name in one namespace is a fatal error, so a collision means a
    /// conditional declaration, a platform-exclusive twin, or an extraction
    /// bug — and merging them would let one declaration's sites stand in for
    /// another's.
    fn mergeable(&self, _a: &Definition, _b: &Definition) -> bool {
        false
    }

    fn scope(
        &self,
        _cfg: &PhpProject,
        _file: &FileFacts<PhpLang>,
        _probe: &dyn SymbolProbe,
    ) -> PhpScope {
        PhpScope
    }

    /// Empty. Tier 2 emits no inheritance reference, so there is no supertype
    /// relation to derive and no member lookup that would walk one.
    fn link_kinds(&self) -> &'static [RefKind] {
        &[]
    }

    fn resolve(
        &self,
        cfg: &PhpProject,
        _scope: &PhpScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        let mut p = Probes {
            table: probe,
            seen: Vec::new(),
        };
        let outcome = match r.kind {
            RefKind::Import => Self::resolve_import(cfg, r, &mut p),
            // Structurally unreachable: this track's extractor emits one
            // reference kind. Kept because `resolve` is total over
            // `Reference`, and the honest answer for a site a tier-2 language
            // does not link is the reason named for exactly that.
            _ => Outcome::Unresolved(UnresolvedReason::TierTwoLanguage),
        };
        Resolution {
            outcome,
            candidates: p.seen,
        }
    }
}

/// The PHP track's scan entry point, reading every `.php` the walk finds.
pub fn scan_php(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_php_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_php`] under a repository's include/exclude globs. What
/// [`crate::track_php::TRACK`] holds.
pub fn scan_php_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<PhpLang>(root, db, &PhpExtractor, &PhpResolver, filter)
}

/// PHP's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(PhpLang::LANG, crate::model::Lang::Php));
    assert!(matches!(PhpLang::DOMAIN, crate::model::Domain::Php));
};

/// The extractor's `Extractor` impl is what the driver runs; `extract` is
/// what the fixtures call. Naming both keeps the trait object honest.
const _: fn() = || {
    fn assert_extractor<T: Extractor<PhpLang>>() {}
    assert_extractor::<PhpExtractor>();
};
