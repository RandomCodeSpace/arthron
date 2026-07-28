//! HCL extractor: one file in, records out. Forbidden from linking.
//!
//! The YAML rule (embedded from `rules/hcl.yml`) selects `block` nodes; this
//! module decides which of them Terraform gives meaning to, and reads their
//! labels.
//!
//! # What a best-effort tier-2 extractor emits, and what it must not
//!
//! Definitions and structure, plus **one import-like reference and nothing
//! else**. HCL has no import statement: the only site in a `.tf` file that
//! names something declared elsewhere is a `module` block's `source`
//! attribute, and what it names is a *directory*.
//!
//! Everything else Terraform writes is expression-level, and none of it is
//! emitted. The corpus contains 750 `var.<name>` references against 236
//! `variable` blocks, 191 `local.` references, 1,188
//! `module.<name>.<output>` references and every `<type>.<name>` resource
//! address besides — all of them resolvable in principle, none of them an
//! import, and all of them out of scope at tier 2. Emitting them would put
//! references into a denominator this track does not link, which is tier-1
//! coverage claimed without tier-1 work. The rate this track reports is over
//! `module` sources alone, and the definition census beside it is the other
//! half of what tier 2 promises.
//!
//! # What a definition is here
//!
//! One per top-level block Terraform addresses, plus one per attribute of a
//! `locals` block, plus the file's own directory. **Top-level only**: a
//! `block` node matches at every depth, and Terraform gives block types
//! meaning only at the top of a file — `provider_meta "aws"` inside
//! `terraform` is not a provider, `content` inside `dynamic` is not
//! anything, and a nested block spelled `variable` is an argument to whatever
//! contains it.
//!
//! # Recorded non-emissions
//!
//! Each is a deliberate refusal, written down rather than left to be
//! rediscovered:
//!
//! - **`terraform { required_providers { … } }` is not an import.** Its
//!   `source` sits in the same syntactic position as a module's and names a
//!   different namespace — a *provider* registry address such as
//!   `hashicorp/aws`, which is never a directory and never in this
//!   repository. The resolver beside this file indexes directories and
//!   grades module sources; a provider address is not a question it can
//!   answer, and that namespace mismatch is the whole of the reason.
//!   Measured rather than assumed, because the direction matters: a provider
//!   address is two slash-segments and the module resolver's package grammar
//!   accepts three or four, so each of the sixteen `versions.tf` files would
//!   contribute one [`crate::UnresolvedReason::ModuleNotFound`] *inside* the
//!   denominator, not one [`crate::Outcome::External`] row outside it — 23
//!   resolved of 39, and the gated rate would read 59.0% instead of 100.0%.
//!   So this refusal is the direction that flatters the rate, and it is
//!   taken on the namespace ground alone; recorded plainly here, because a
//!   rejected alternative whose stated reason points the other way is worse
//!   than none. A track that wants providers owes them a provider-address
//!   grammar and a bucket of their own, not a pass through this resolver.
//!   HCL has no dependency manifest separate from its source, so this is the
//!   manifest half of the file, and it is read by nothing here.
//! - **`provider "aws" { … }` declares no node.** A provider configuration is
//!   addressed `aws.alias` from a `provider =` meta-argument, which is
//!   expression-level. The block is structure.
//! - **`moved`, `import`, `check` and `removed` blocks.** Each names an
//!   existing address rather than declaring one; the naming is
//!   expression-level and out of tier-2 scope, and none appears in the
//!   corpus.
//! - **A block whose labels Terraform would reject** — the wrong count, or a
//!   label that is not a plain literal — declares nothing. No address can be
//!   composed for it, and an invented one would be a node nothing can name.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_hcl::lang::{HclLang, dir_name, dir_of};

/// The embedded HCL extraction rules.
const HCL_RULES: &str = include_str!("../rules/hcl.yml");

/// How a `module` block spells the thing it names.
///
/// The distinction is the whole of what the extractor may say about a
/// source: whether the file states one literally. *Which* module a literal
/// names — a directory on disk or a package outside this repository — is a
/// linking decision, and linking is the resolver's alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceForm {
    /// `source = "<text>"` — one plain string, quotes stripped.
    Literal(String),
    /// The source is not a plain string: interpolated, computed, a heredoc,
    /// or a bare name. Never guessed.
    Dynamic,
}

impl SourceForm {
    /// The form's name, for a census that has to distinguish them.
    pub fn name(&self) -> &'static str {
        match self {
            SourceForm::Literal(_) => "literal",
            SourceForm::Dynamic => "dynamic",
        }
    }
}

/// One `module` block's `source` attribute: what it spells plus where it
/// sits.
///
/// Every `ModuleSource` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is how the
/// resolver pairs the two without the core learning what a `source` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSource {
    /// What the attribute spells.
    pub form: SourceForm,
    /// Where the attribute sits. The whole `source = …`, so the key is
    /// unique within the file.
    pub span: Span,
}

/// Per-file HCL facts only the HCL resolver reads.
///
/// `rel_path` is here because a `source` is resolved against *where the file
/// is*, and the core must not be the layer that turns a path into a module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HclHeader {
    /// Repo-relative, `/`-separated path of the file.
    pub rel_path: String,
    /// Every `module` source attribute, in source order.
    pub sources: Vec<ModuleSource>,
}

/// The HCL extractor.
pub struct HclExtractor;

impl Extractor<HclLang> for HclExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<HclLang> {
        extract(rel_path, source)
    }
}

/// The plain text of a `string_lit`, or `None` when it is not plain: an
/// interpolation or a template directive makes the value a runtime fact.
///
/// An empty string is `Some("")` — a literal that spells nothing is still a
/// literal, and the two are different answers.
fn literal_text(node: &SgNode) -> Option<String> {
    let mut out = String::new();
    for child in node.children() {
        match &*child.kind() {
            "quoted_template_start" | "quoted_template_end" => {}
            "template_literal" => out.push_str(&child.text()),
            _ => return None,
        }
    }
    Some(out)
}

/// A block's type and its labels, as written. `None` when a label is not a
/// plain literal.
fn head(node: &SgNode) -> Option<(String, Vec<String>)> {
    let mut children = node.children();
    let block_type = children.next()?;
    if block_type.kind() != "identifier" {
        return None;
    }
    let mut labels = Vec::new();
    for child in children {
        match &*child.kind() {
            // HCL's native syntax allows a bare identifier where Terraform's
            // own style writes a quoted string. Both are the same label.
            "identifier" => labels.push(child.text().to_string()),
            "string_lit" => labels.push(literal_text(&child)?),
            // `block_start`: the labels are over.
            _ => break,
        }
    }
    Some((block_type.text().to_string(), labels))
}

/// Whether this block sits at the top level of its file.
///
/// The one structural judgement in this module, and the reason it is here:
/// Terraform gives a block type meaning only at the top of a file, and a
/// `block` node matches at every depth.
fn top_level(node: &SgNode) -> bool {
    node.parent()
        .filter(|body| body.kind() == "body")
        .and_then(|body| body.parent())
        .is_some_and(|file| file.kind() == "config_file")
}

/// The immediate attributes of a block's body, in source order.
fn attributes<'r>(node: &SgNode<'r>) -> Vec<SgNode<'r>> {
    node.children()
        .find(|c| c.kind() == "body")
        .map(|body| {
            body.children()
                .filter(|c| c.kind() == "attribute")
                .collect()
        })
        .unwrap_or_default()
}

/// An attribute's name, when it has one.
fn attribute_name(attr: &SgNode) -> Option<String> {
    attr.children()
        .next()
        .filter(|c| c.kind() == "identifier")
        .map(|c| c.text().to_string())
}

/// What an attribute's value spells: one plain string, or anything else.
fn value_form(attr: &SgNode) -> SourceForm {
    let plain = attr
        .children()
        .find(|c| c.kind() == "expression")
        .filter(|expr| expr.children().count() == 1)
        .and_then(|expr| expr.children().find(|c| c.kind() == "literal_value"))
        .filter(|value| value.children().count() == 1)
        .and_then(|value| value.children().find(|c| c.kind() == "string_lit"))
        .and_then(|lit| literal_text(&lit));
    match plain {
        Some(text) => SourceForm::Literal(text),
        None => SourceForm::Dynamic,
    }
}

/// One definition record.
fn def(kind: DefKind, owner: Vec<String>, name: String, span: Span) -> Definition {
    Definition {
        kind,
        name,
        owner,
        space: DeclSpace::Value,
        facets: DefFacets::default(),
        params: None,
        span,
    }
}

/// The `(kind, owner, name)` a top-level block declares, or `None` when it
/// declares nothing: a block type Terraform addresses no object under, or a
/// label list Terraform would reject.
///
/// The owner chain is the address prefix, so the FQN grammar composes it by
/// joining — see [`crate::track_hcl::lang`] for the table and for why a
/// managed resource carries a prefix its own expression syntax does not.
fn declared(block_type: &str, labels: &[String]) -> Option<(DefKind, Vec<String>, String)> {
    if labels.iter().any(String::is_empty) {
        return None;
    }
    let owner = |segments: &[&str]| segments.iter().map(|s| (*s).to_string()).collect();
    match (block_type, labels.len()) {
        // A managed resource and a data source: a type label and a name.
        ("resource", 2) => Some((
            DefKind::Var,
            owner(&["resource", &labels[0]]),
            labels[1].clone(),
        )),
        ("data", 2) => Some((
            DefKind::Var,
            owner(&["data", &labels[0]]),
            labels[1].clone(),
        )),
        // An input variable, and a module call: one name each.
        ("variable", 1) => Some((DefKind::Var, owner(&["var"]), labels[0].clone())),
        ("module", 1) => Some((DefKind::Var, owner(&["module"]), labels[0].clone())),
        // An output value is the module's whole exported surface, reached
        // from outside as `module.<call>.<name>` — a field of the module
        // object, which is the kind it is filed under.
        ("output", 1) => Some((DefKind::Field, owner(&["output"]), labels[0].clone())),
        _ => None,
    }
}

/// Extract one HCL file.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<HclLang> {
    static RULES: OnceLock<Rules> = OnceLock::new();
    let rules = RULES.get_or_init(|| Rules::compile(HCL_RULES).expect("hcl.yml compiles"));

    let dir = dir_of(rel_path);
    let mut facts: FileFacts<HclLang> = FileFacts {
        header: HclHeader {
            rel_path: rel_path.to_string(),
            sources: Vec::new(),
        },
        defs: Vec::new(),
        refs: Vec::new(),
    };

    // The file's own container, first, because the driver reads the first
    // `Module` definition as the file's own. Every `.tf` file declares its
    // directory whether or not it declares anything else: the corpus's
    // thirteen zero-byte `variables.tf` files each do exactly that, and an
    // empty file is a file rather than an error.
    facts.defs.push(Definition {
        kind: DefKind::Module,
        name: dir_name(dir).to_string(),
        owner: Vec::new(),
        space: DeclSpace::Namespace,
        facets: DefFacets::SYNTHETIC,
        params: None,
        span: Span {
            byte_start: 0,
            byte_end: source.len() as u32,
            line: 1,
        },
    });

    let tree = SourceTree::parse_hcl(source);
    for (_, node) in tree.matches(rules) {
        if !top_level(&node) {
            continue;
        }
        let Some((block_type, labels)) = head(&node) else {
            continue;
        };
        // A `locals` block has no label and declares nothing itself: each of
        // its attributes is a local value, and `local.<name>` is what a
        // reference elsewhere spells.
        if block_type == "locals" && labels.is_empty() {
            for attr in attributes(&node) {
                let Some(name) = attribute_name(&attr) else {
                    continue;
                };
                facts.defs.push(def(
                    DefKind::Const,
                    vec!["local".to_string()],
                    name,
                    span_of(&attr),
                ));
            }
            continue;
        }
        let Some((kind, owner, name)) = declared(&block_type, &labels) else {
            continue;
        };
        facts
            .defs
            .push(def(kind, owner.clone(), name.clone(), span_of(&node)));
        if block_type == "module" {
            module_source(&mut facts, &node, &name);
        }
    }
    // One rule, so the matches already arrive in tree order; sorted anyway
    // because source order is what a reader of a report expects and what a
    // span-keyed pairing needs to be stable under.
    facts.defs[1..].sort_by_key(|d| d.span.byte_start);
    facts.refs.sort_by_key(|r| r.span.byte_start);
    facts.header.sources.sort_by_key(|s| s.span.byte_start);
    facts
}

/// The one import-like reference HCL has: a `module` block's `source`.
///
/// A `module` block with no `source` at all contributes none — invalid
/// Terraform, and a reference to the empty string would be worse than
/// nothing. An empty *literal* does contribute one: it is a literal that
/// names no module, which the resolver has an answer for.
fn module_source(facts: &mut FileFacts<HclLang>, node: &SgNode, call: &str) {
    let Some(attr) = attributes(node)
        .into_iter()
        .find(|a| attribute_name(a).as_deref() == Some("source"))
    else {
        return;
    };
    let span = span_of(&attr);
    let form = value_form(&attr);
    let raw_target = match &form {
        SourceForm::Literal(text) => text.clone(),
        // What the file wrote, since there is no literal to quote.
        SourceForm::Dynamic => attr
            .children()
            .find(|c| c.kind() == "expression")
            .map(|e| e.text().to_string())
            .unwrap_or_default(),
    };
    facts.header.sources.push(ModuleSource { form, span });
    facts.refs.push(Reference {
        kind: RefKind::Import,
        // A module source names a container, never a declaration in one.
        space: DeclSpace::Namespace,
        raw_target: raw_target.clone(),
        target: RefTarget {
            root: TargetRoot::Name,
            segments: vec![raw_target],
        },
        // Structurally false. Tier 2 emits no expression-level reference, so
        // nothing here can name a local, and `LocalBinding` does not apply to
        // this track.
        locally_bound: false,
        argc: None,
        // The edge starts at the module call, which is a definition this same
        // file declares — so its address is what the encloser spells, and
        // `Resolver::def_fqn` reads it back into the same identity.
        enclosing: Some(Encloser {
            path: vec!["module".to_string(), call.to_string()],
            kind: DefKind::Var,
        }),
        span,
    });
}
