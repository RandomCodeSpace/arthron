//! The one place an Elixir [`crate::Outcome`] is produced. Never drops.
//!
//! # The gate is an import-resolution rate
//!
//! Elixir is a **tier-2, best-effort** language here: definitions, structure
//! and import-like references, and no verified call edges. So the references
//! this resolver classifies are exactly the `alias`, `import`, `require` and
//! `use` directives the extractor emits, and the rate the gate defends is the
//! share of them that name a module this repository declares. Every one ends
//! `Resolved`, `External`, or `Unresolved` with a reason — there is no way to
//! express "dropped".
//!
//! # The import model
//!
//! **Nothing in Elixir names a file.** There is no import-by-path form in the
//! language: a directive names a module by the atom it compiles to, and the
//! compiler finds it in the whole application. So resolution here is a probe
//! and never a search, and [`ElixirScope`] carries only the clause each
//! reference belongs to.
//!
//! Two rules:
//!
//! 1. **A module this repository declares** ⇒ [`crate::Outcome::Resolved`].
//!    The in-repository module set is every `defmodule`, `defprotocol` and
//!    `defimpl` in every file the walk read, composed through the modules
//!    that enclose them — see [`crate::track_elixir::extract`], where 59 of
//!    the corpus's 142 modules get a name that appears nowhere in their own
//!    source.
//! 2. **A module it does not declare is declared somewhere else** ⇒
//!    [`crate::Outcome::External`], named by the whole module name. In a
//!    corpus that compiles there is no third possibility for three of the
//!    four directives: `import`, `require` and `use` all require the module
//!    to exist and be compiled, so a name absent from a complete
//!    in-repository set is Elixir's own standard library, OTP, or a hex
//!    dependency under `deps/` — none of which this scan indexes.
//!
//! A target that is not a literal module name — `require unquote(target)`,
//! `alias __MODULE__.Sub` — resolves against nothing:
//! [`crate::UnresolvedReason::DynamicModuleSpecifier`], never a guess.
//!
//! # Why the external node is the whole module name
//!
//! C# files an external `using` under its root namespace segment, because a
//! namespace is a hierarchy and the root is the coarsest unit its name
//! resolution keys on. **Elixir has no such hierarchy.** `Plug.Crypto` is one
//! atom, `Elixir.Plug.Crypto`, and it is no more a child of `Plug` than
//! `Plugin` is; this repository declares `Plug` and does not declare
//! `Plug.Crypto`, and both facts are true at once. So the root segment would
//! be a guess about ownership the language does not have, and the module name
//! itself is the exact and only honest identity.
//!
//! # What `External` costs, and the two things that keep it honest
//!
//! `External` sits outside **both** terms of the resolution rate, so a rule
//! that widens it raises the rate without linking anything. That makes rule 2
//! the most dangerous line in this track, and it is the finding the earlier
//! batches wrote down — an in-repository module filed as somebody else's
//! vanishes from the measurement instead of failing it.
//!
//! - **The set it is measured against is complete and measured, not
//!   remembered.** There is no standard-library list in this track, no
//!   package-name-to-module-prefix table, and nothing read out of `mix.exs`.
//!   Ruby refuses `External` for exactly the opposite situation: it *searches*
//!   a load path, so it cannot tell its standard library from a load root it
//!   got wrong. Elixir searches nothing.
//! - **A name is only absolute once the file's own bindings are applied.**
//!   `alias Plug.Conn` binds `Conn`, so `import Conn` names `Plug.Conn` and
//!   resolves. Without that composition it would name a module called `Conn`,
//!   miss, and be filed as external — the laundering failure in its Elixir
//!   spelling. The composition is the extractor's, and
//!   `tests/elixir_extract.rs` holds it.
//!
//! Beyond those two, the `external` count is itself a baseline field: any
//! drift in it fails the gate and has to be re-based deliberately, and
//! `tests/elixir_corpus.rs` pins the external module set **by name**, so a
//! composition bug that pushed an in-repository module out of the
//! measurement shows up as an addition to that list rather than as a quietly
//! better rate.
//!
//! # Known limits, recorded rather than left to be rediscovered
//!
//! - **`alias` does not prove existence.** `import M`, `require M` and
//!   `use M` all fail to compile if `M` is not there; `alias M.N` is a purely
//!   lexical binding and compiles whether or not `M.N` exists anywhere. So a
//!   dead alias is filed `External` rather than as a miss. The corpus writes
//!   none — every one of its 24 aliases names a module that exists — and the
//!   pinned external list is what would surface one.
//! - **`LocalBinding` does not apply.** Tier 2 emits no expression-level
//!   reference, so no Elixir reference can name a variable or a parameter.
//!   The bucket stays empty, and the baseline records it as zero — which
//!   makes this rate un-gameable by the one reclassification the rate's own
//!   definition permits.
//! - **A module a macro generates.** `Module.concat/2` and friends mint
//!   module names at compile time; nothing here expands a macro, so such a
//!   module is neither declared nor resolvable. The same limit covers this
//!   corpus's 71 `use` sites, whose expansions inject `import` and `def`
//!   forms — the count this track measures, not the count a grep of `use`
//!   lines returns, which is 94 and includes the documentation.

use std::collections::HashMap;
use std::path::Path;

use crate::lang::{
    Extractor, FileFacts, FileIndex, Language, LayoutError, Resolution, Resolver, SymbolProbe,
};
use crate::model::{DefKind, Definition, Fqn, NodeId, RefKind, Reference, node_id};
use crate::track_elixir::extract::{ElixirExtractor, ElixirHeader, ImportForm};
use crate::track_elixir::lang::{ElixirLang, ElixirProject, field_key, function_key, member_fqn};
use crate::{Outcome, UnresolvedReason};

/// One file's view of what its own directives mean: the clause each
/// reference belongs to, keyed by the span the two share.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElixirScope {
    imports: HashMap<(u32, u32), ImportForm>,
}

/// Elixir's resolver. Stateless: everything it reads is in the scope or the
/// probe.
pub struct ElixirResolver;

impl Resolver<ElixirLang> for ElixirResolver {
    /// Phase 0 reads nothing. See [`ElixirProject`].
    fn config(&self, _root: &Path, _files: &FileIndex) -> Result<ElixirProject, LayoutError> {
        Ok(ElixirProject)
    }

    /// Empty: no manifest decides any identity here, so no manifest can
    /// invalidate a store.
    fn config_digest(&self, _cfg: &ElixirProject) -> Vec<u8> {
        Vec::new()
    }

    /// `None`: an Elixir identity is decided by the `defmodule`s the file
    /// itself writes, so both phases build the same names from the same
    /// bytes and there is nothing to learn from the store. A file may declare
    /// six modules — three in the corpus do — and this asks for one.
    fn declared_container(
        &self,
        _cfg: &ElixirProject,
        _header: &ElixirHeader,
    ) -> Option<(String, String)> {
        None
    }

    /// Nothing to learn, for the reason [`Resolver::declared_container`]
    /// gives.
    fn learn_containers(&self, _cfg: &mut ElixirProject, _names: &HashMap<String, String>) {}

    /// Every file the walk reached. There is no nested-manifest fence: an
    /// umbrella project holds a `mix.exs` per application and every one of
    /// them is this repository's own code. What is genuinely not ours is
    /// `deps/` and `_build/`, and those are pruned from the walk by
    /// [`ElixirLang::skip_dirs`] rather than filtered out of it.
    fn owns_file(&self, _cfg: &ElixirProject, _rel_path: &str) -> bool {
        true
    }

    fn def_fqn(
        &self,
        _cfg: &ElixirProject,
        _header: &ElixirHeader,
        owner: &[String],
        def: &Definition,
        _probe: &dyn SymbolProbe,
    ) -> Option<Fqn> {
        if def.name.is_empty() {
            return None;
        }
        if def.kind == DefKind::Module {
            return Some(Fqn::new(crate::track_elixir::lang::module_fqn(
                owner, &def.name,
            )));
        }
        // Elixir has no declaration outside a module, so an owner that names
        // none is not a shape this extractor produces and not one a name is
        // invented for.
        let module = owner.first()?;
        let key = match def.kind {
            DefKind::Field => field_key(&def.name),
            // An encloser arrives as a synthetic definition carrying only a
            // path, so its name is already `f/2` and its `params` are gone.
            // Reading both is what keeps the identity an edge starts at equal
            // to the one the definition phase filed. `/` is in no Elixir
            // function name, so the two spellings cannot be confused.
            _ if def.name.contains('/') => def.name.clone(),
            _ => function_key(&def.name, def.params.as_ref().map_or(0, |p| p.count)),
        };
        Some(Fqn::new(member_fqn(module, &key)))
    }

    /// Empty: Elixir reaches every node by its FQN alone. A module name is an
    /// atom with no second spelling, and an `alias` binds a name inside one
    /// file and is nameable from nowhere else.
    fn index_keys(&self, _cfg: &ElixirProject, _fqn: &Fqn, _def: &Definition) -> Vec<NodeId> {
        Vec::new()
    }

    /// A module is a **definition**, never a package node.
    ///
    /// The default files every [`DefKind::Module`] as a package, on the rule
    /// that a container reopened by every file under it is not a collision.
    /// Elixir has no such container: a module is declared by exactly one
    /// `defmodule`, two of them under one name is a redefinition the compiler
    /// warns about, and filing modules as packages would make that
    /// uncountable — which is the finding Scala recorded for `object`.
    fn stores_as_package(&self, _def: &Definition) -> bool {
        false
    }

    /// Two Elixir declarations that agree on kind, name, owner and arity are
    /// one entity, and the language writes them apart routinely:
    ///
    /// - **Clauses.** `def call(%Plug.Conn{} = conn, opts)` written five
    ///   times with five patterns is one function with five clauses.
    /// - **A head and its clauses.** `def f(a, b \\ 1)` beside `def f(a, b)`.
    /// - **A conditional definition.** The corpus's own `mix.exs` writes
    ///   `defp plug_crypto_version` once per branch of an `if`, and both
    ///   arms are read.
    ///
    /// **A module is never mergeable**, and that is the other half of the
    /// rule. `defmodule Plug.Conn` written twice — in one file or in two — is
    /// a redefinition the compiler warns about and the later one wins, which
    /// is exactly the silent overwrite [`crate::store::Report::fqn_collisions`]
    /// exists to surface. Merging the pair would keep both declaration sites
    /// on one node and count nothing.
    fn mergeable(&self, a: &Definition, b: &Definition) -> bool {
        if a.kind == DefKind::Module || b.kind == DefKind::Module {
            return false;
        }
        a.kind == b.kind && a.name == b.name && a.owner == b.owner && a.params == b.params
    }

    fn scope(
        &self,
        _cfg: &ElixirProject,
        file: &FileFacts<ElixirLang>,
        _probe: &dyn SymbolProbe,
    ) -> ElixirScope {
        ElixirScope {
            imports: file
                .header
                .imports
                .iter()
                .map(|i| ((i.span.byte_start, i.span.byte_end), i.form.clone()))
                .collect(),
        }
    }

    /// Empty. Tier 2 emits no inheritance reference — `@behaviour Plug` is
    /// part of a module's structure here and is not resolved — so there is no
    /// supertype relation to derive and no member lookup that would walk one.
    fn link_kinds(&self) -> &'static [RefKind] {
        &[]
    }

    fn resolve(
        &self,
        _cfg: &ElixirProject,
        scope: &ElixirScope,
        r: &Reference,
        probe: &dyn SymbolProbe,
    ) -> Resolution {
        match scope.imports.get(&(r.span.byte_start, r.span.byte_end)) {
            Some(ImportForm::Module(path)) => {
                let fqn = path.join(".");
                let id = node_id(ElixirLang::DOMAIN, &fqn);
                let outcome = if probe.probe(&id).is_some() {
                    Outcome::Resolved(id)
                } else {
                    // Rule 2. The in-repository module set is complete, so a
                    // name absent from it is declared by the standard
                    // library, by OTP, or by a dependency this scan does not
                    // read — and the module name is exactly what names it.
                    Outcome::External(fqn)
                };
                Resolution {
                    outcome,
                    candidates: vec![id],
                }
            }
            // A clause whose target could not be read as one module name,
            // and — unreachable, since the extractor emits a clause and its
            // reference together — a reference with no clause at all. Both
            // mean the same thing: this build cannot say which module is
            // named, and it will not guess one.
            Some(ImportForm::Dynamic) | None => Resolution {
                outcome: Outcome::Unresolved(UnresolvedReason::DynamicModuleSpecifier),
                candidates: Vec::new(),
            },
        }
    }
}

/// The Elixir track's scan entry point, reading every `.ex` and `.exs` the
/// walk finds.
pub fn scan_elixir(root: &Path, db: &Path) -> Result<crate::store::Report, String> {
    scan_elixir_with(root, db, &crate::config::FileFilter::none())
}

/// [`scan_elixir`] under a repository's include/exclude globs. What
/// [`crate::track_elixir::TRACK`] holds.
pub fn scan_elixir_with(
    root: &Path,
    db: &Path,
    filter: &crate::config::FileFilter,
) -> Result<crate::store::Report, String> {
    crate::pipeline::scan::<ElixirLang>(root, db, &ElixirExtractor, &ElixirResolver, filter)
}

/// Elixir's `Lang` and `Domain`, restated where a reader of the resolver will
/// look for them.
const _: () = {
    assert!(matches!(ElixirLang::LANG, crate::model::Lang::Elixir));
    assert!(matches!(ElixirLang::DOMAIN, crate::model::Domain::Elixir));
};

/// The extractor's `Extractor` impl is what the driver runs;
/// [`crate::track_elixir::extract::extract`] is what the fixtures call.
/// Naming both keeps the trait object honest.
const _: fn() = || {
    fn assert_extractor<T: Extractor<ElixirLang>>() {}
    assert_extractor::<ElixirExtractor>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeclSpace, DefFacets, Domain, Span};
    use crate::track_elixir::extract::extract;
    use std::collections::HashSet;

    fn def_of(kind: DefKind, name: &str, owner: &[&str], arity: Option<u32>) -> Definition {
        Definition {
            kind,
            name: name.to_string(),
            owner: owner.iter().map(|o| (*o).to_string()).collect(),
            space: DeclSpace::Value,
            facets: DefFacets::default(),
            params: arity.map(|count| crate::model::Params {
                count,
                varargs: false,
                types: Vec::new(),
            }),
            span: Span {
                byte_start: 0,
                byte_end: 0,
                line: 1,
            },
        }
    }

    fn header() -> ElixirHeader {
        ElixirHeader {
            rel_path: "lib/plug/conn.ex".to_string(),
            imports: Vec::new(),
        }
    }

    fn fqn(owner: &[&str], def: &Definition) -> Option<String> {
        let table: HashSet<NodeId> = HashSet::new();
        let owner: Vec<String> = owner.iter().map(|o| (*o).to_string()).collect();
        ElixirResolver
            .def_fqn(&ElixirProject, &header(), &owner, def, &table)
            .map(Fqn::into_string)
    }

    #[test]
    fn a_nested_module_is_named_by_the_chain_that_encloses_it() {
        let def = def_of(DefKind::Module, "InvalidCSRFTokenError", &[], None);
        assert_eq!(
            fqn(&["Plug.CSRFProtection"], &def),
            Some("Plug.CSRFProtection.InvalidCSRFTokenError".to_string()),
        );
    }

    #[test]
    fn a_function_is_named_by_its_arity_and_a_field_by_its_key() {
        let f = def_of(DefKind::Function, "get_req_header", &["Plug.Conn"], Some(2));
        assert_eq!(
            fqn(&["Plug.Conn"], &f),
            Some("Plug.Conn#get_req_header/2".to_string())
        );
        let field = def_of(DefKind::Field, "host", &["Plug.Conn"], None);
        assert_eq!(
            fqn(&["Plug.Conn"], &field),
            Some("Plug.Conn#%host".to_string())
        );
    }

    #[test]
    fn an_enclosers_key_spells_the_same_identity_the_definition_was_filed_under() {
        // What `Encloser::as_definition` hands back for a reference inside
        // `def call/2`: a plain definition whose name already carries the
        // arity and whose `params` are gone. It must name the node the
        // definition phase filed.
        let from_encloser = def_of(DefKind::Function, "call/2", &["Plug.Conn"], None);
        let from_definition = def_of(DefKind::Function, "call", &["Plug.Conn"], Some(2));
        assert_eq!(
            fqn(&["Plug.Conn"], &from_encloser),
            fqn(&["Plug.Conn"], &from_definition),
        );
    }

    #[test]
    fn a_declaration_with_no_module_is_not_nameable() {
        let f = def_of(DefKind::Function, "loose", &[], Some(0));
        assert_eq!(fqn(&[], &f), None);
    }

    #[test]
    fn a_module_is_a_definition_so_two_files_declaring_one_is_a_collision() {
        let m = def_of(DefKind::Module, "Plug.Conn", &[], None);
        // Not a package node, so the store asks about the pair at all...
        assert!(!ElixirResolver.stores_as_package(&m));
        // ...and not mergeable, so it is counted rather than folded away.
        let other = def_of(DefKind::Module, "Plug.Conn", &[], None);
        assert!(!ElixirResolver.mergeable(&m, &other));
        let f = def_of(DefKind::Function, "Plug.Conn", &["A"], Some(0));
        assert!(!ElixirResolver.mergeable(&m, &f));
    }

    #[test]
    fn the_clauses_of_one_function_are_one_entity() {
        // `def call(%Plug.Conn{} = conn, opts)` written five times with five
        // patterns is one function, and so is a `defp` written once per
        // branch of an `if` — which the corpus's own `mix.exs` does.
        let a = def_of(DefKind::Function, "call", &["Plug.Conn"], Some(2));
        let b = def_of(DefKind::Function, "call", &["Plug.Conn"], Some(2));
        assert!(ElixirResolver.mergeable(&a, &b));
    }

    #[test]
    fn two_arities_of_one_name_are_two_entities() {
        let one = def_of(DefKind::Function, "get", &["Plug.Conn"], Some(1));
        let two = def_of(DefKind::Function, "get", &["Plug.Conn"], Some(2));
        assert!(!ElixirResolver.mergeable(&one, &two));
        assert_ne!(fqn(&["Plug.Conn"], &one), fqn(&["Plug.Conn"], &two));
    }

    #[test]
    fn every_directive_reference_is_paired_with_a_clause() {
        // The pairing is by span, so a reference the scope cannot find would
        // silently become `DynamicModuleSpecifier` for a perfectly literal
        // target. It must be total, including for a tuple clause whose two
        // references come from one call.
        let table: HashSet<NodeId> = HashSet::new();
        let source = "defmodule A do\n  alias Plug.{Conn, Router}\n  import Plug.Test\n  \
                      require unquote(x)\n  use Plug.Builder\nend\n";
        let facts = extract("lib/a.ex", source);
        let scope = ElixirResolver.scope(&ElixirProject, &facts, &table);
        assert_eq!(facts.refs.len(), 5);
        for r in &facts.refs {
            assert!(
                scope
                    .imports
                    .contains_key(&(r.span.byte_start, r.span.byte_end)),
                "unpaired: {}",
                r.raw_target,
            );
        }
    }

    #[test]
    fn an_external_module_is_named_by_the_whole_module_name() {
        let table: HashSet<NodeId> = HashSet::new();
        let facts = extract(
            "test/a_test.exs",
            "defmodule T do\n  use ExUnit.Case\nend\n",
        );
        let scope = ElixirResolver.scope(&ElixirProject, &facts, &table);
        let got = ElixirResolver.resolve(&ElixirProject, &scope, &facts.refs[0], &table);
        assert_eq!(got.outcome, Outcome::External("ExUnit.Case".to_string()));
        // The probe really was made: a candidate that was never read cannot
        // wake this file when the module it named starts existing.
        assert_eq!(got.candidates, vec![node_id(Domain::Elixir, "ExUnit.Case")]);
    }
}
