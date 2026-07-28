//! Bash extractor: one file in, records out. Forbidden from linking.
//!
//! YAML rules (embedded from `rules/bash.yml`) select nodes by kind; this
//! module interprets their fields.
//!
//! # What a best-effort tier-2 extractor emits, and what it must not
//!
//! Definitions and structure, plus **`source` references and nothing else**.
//! Bash's gate is an import-resolution rate, so a call site emitted here
//! would enter a denominator nothing in this track can resolve — tier-1
//! coverage claimed without tier-1 measurement. That bites harder in shell
//! than anywhere else: in bash a call is an ordinary `command` node, spelled
//! exactly like `ls` or `printf`, so *every* command in the tree would become
//! a reference and the rate would become a statement about how much of
//! coreutils is in the repository. None is emitted.
//!
//! # Recorded under-counts
//!
//! Each is a known shortfall, written down rather than left to be
//! rediscovered, and none may be closed by widening a bucket:
//!
//! - **A `source` behind a wrapper.** `builtin source x`, `command . x` and
//!   `eval source x` all really do source a file. Reading them means a second
//!   command model — which head is the real one — and nothing in the measured
//!   corpus writes one.
//! - **A global variable.** Bash has no declaration syntax for one: `X=1` is
//!   a write, and a node per write is several nodes for one slot. The forms
//!   that *are* declarations — `readonly X=…`, `declare -r X=…`, `declare -g
//!   X=…` — are not read either, because the measured corpus contains none at
//!   file scope (its 151 `declaration_command`s are 138 `local`, which is a
//!   local binding and not a node by design, plus 13 `export`, of which the
//!   one at file scope is `export -f tput` and re-exports a function that
//!   already has a node). Implementing on an unexercised shape is how a
//!   census starts lying.
//! - **`alias`.** An alias declares a name, and it is expanded only in
//!   interactive shells unless `expand_aliases` is set — a runtime fact this
//!   build cannot read. The corpus writes none.
//! - **A function whose name is not a literal.** There is no such thing in
//!   bash's grammar; the name field of a `function_definition` is a `word`.
//!   Recorded so the absence is a fact rather than an oversight.

use std::sync::OnceLock;

use crate::lang::{Extractor, FileFacts};
use crate::model::{
    DeclSpace, DefFacets, DefKind, Definition, Encloser, RefKind, RefTarget, Reference, Span,
    TargetRoot,
};
use crate::sg::{Rules, SgNode, SourceTree, span_of};
use crate::track_bash::lang::BashLang;

/// The embedded Bash extraction rules.
const BASH_RULES: &str = include_str!("../rules/bash.yml");

/// The two spellings of the one builtin that sources a file.
const SOURCE_COMMANDS: [&str; 2] = ["source", "."];

/// How a `source` clause spells the file it names.
///
/// The distinction is the whole of bash's import model: a specifier the shell
/// would expand names a file only the running shell knows, and a specifier it
/// would not is a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceForm {
    /// The argument is one plain literal path, however it was quoted.
    Literal(String),
    /// The argument is not a literal: an expansion, a substitution, a glob, a
    /// brace list, a leading tilde, or an escape. Never guessed.
    Dynamic,
}

/// One `source` clause: what it spells plus where it sits.
///
/// Every `SourceSpec` shares its [`Span`] with exactly one
/// [`RefKind::Import`] reference in the same [`FileFacts`], which is how the
/// resolver pairs the two without the core learning what a `source` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    /// What the clause spells.
    pub form: SourceForm,
    /// Where the clause sits. The whole command, so the key is unique.
    pub span: Span,
}

/// Per-file Bash facts only the Bash resolver reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BashHeader {
    /// Repo-relative, `/`-separated path of the file.
    pub rel_path: String,
    /// Every `source` clause, in source order.
    pub sources: Vec<SourceSpec>,
}

/// The enclosing function names of a node, outermost first.
///
/// Only `function_definition` is a frame. A subshell, a loop and a
/// conditional are not: each of them changes when a declaration runs, not
/// what it is called, and this scan measures the text.
fn frames(node: &SgNode) -> Vec<String> {
    let mut out: Vec<String> = node
        .ancestors()
        .filter(|a| a.kind() == "function_definition")
        .filter_map(|a| a.field("name").map(|n| n.text().to_string()))
        .collect();
    out.reverse();
    out
}

/// The nearest *nameable* enclosing definition of a reference site.
///
/// A `source` inside a function belongs to that function; one at file scope
/// belongs to nothing, and the driver sources it at the file's own script
/// node.
fn enclosing_definition(node: &SgNode) -> Option<Encloser> {
    let path = frames(node);
    (!path.is_empty()).then_some(Encloser {
        path,
        kind: DefKind::Function,
    })
}

/// The literal value of a `source` argument, or `None` when the shell would
/// expand it into something else.
///
/// Every `None` here becomes [`SourceForm::Dynamic`] and then
/// [`crate::UnresolvedReason::DynamicModuleSpecifier`]: a specifier this
/// function cannot read is one the resolver must refuse to guess, not one it
/// may approximate.
fn literal(node: &SgNode) -> Option<String> {
    match &*node.kind() {
        // An unquoted word. Rejected when it carries anything the shell
        // rewrites: a glob, a brace list, a backslash escape, or the leading
        // tilde that names a home directory.
        "word" => {
            let text = node.text().to_string();
            if text.starts_with('~') || text.contains(['*', '?', '[', ']', '{', '}', '\\']) {
                return None;
            }
            Some(text)
        }
        // Single quotes: every byte between them is itself, and a `'` cannot
        // appear inside.
        "raw_string" => {
            let text = node.text();
            Some(text.strip_prefix('\'')?.strip_suffix('\'')?.to_string())
        }
        // Double quotes: literal only while every child is plain content. An
        // expansion or a substitution is a child of its own kind, and a
        // backslash inside the content is an escape whose value is not the
        // bytes written.
        "string" => {
            let mut out = String::new();
            for child in node.children() {
                match &*child.kind() {
                    "string_content" => {
                        let text = child.text();
                        if text.contains('\\') {
                            return None;
                        }
                        out.push_str(&text);
                    }
                    "\"" => {}
                    _ => return None,
                }
            }
            Some(out)
        }
        // `source 'lib/'util.bash` is one literal written in two pieces.
        "concatenation" => {
            let mut out = String::new();
            for child in node.children() {
                out.push_str(&literal(&child)?);
            }
            Some(out)
        }
        // `$'…'` is a literal only after C-escape processing, which this
        // build does not perform, and every other shape is an expansion.
        _ => None,
    }
}

/// A command's arguments, in order. Assignment prefixes and redirections are
/// not arguments and the grammar already says so.
fn arg_nodes<'r>(command: &SgNode<'r>) -> Vec<SgNode<'r>> {
    command.field_children("argument").collect()
}

/// Extract one Bash file. The whole of the extractor's public surface.
pub fn extract(rel_path: &str, source: &str) -> FileFacts<BashLang> {
    static RULES: OnceLock<Rules> = OnceLock::new();
    let rules = RULES.get_or_init(|| Rules::compile(BASH_RULES).expect("bash.yml compiles"));

    let mut facts: FileFacts<BashLang> = FileFacts {
        header: BashHeader {
            rel_path: rel_path.to_string(),
            sources: Vec::new(),
        },
        defs: Vec::new(),
        refs: Vec::new(),
    };

    // The file's own script node, first, because the driver reads the first
    // `Module` definition as the file's container. Every owned file is a
    // script whether or not it declares a function: a `source` naming an
    // empty file still resolves.
    let stem = rel_path.rsplit('/').next().unwrap_or(rel_path);
    facts.defs.push(Definition {
        kind: DefKind::Module,
        name: stem.to_string(),
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

    let tree = SourceTree::parse_bash(source);
    for (rule, node) in tree.matches(rules) {
        match rule {
            "def-function" => function(&mut facts, &node),
            "command" => command(&mut facts, &node),
            _ => {}
        }
    }
    // Rules run one at a time, so the records arrive rule-major; source order
    // is what a reader of a report expects and what a span-keyed pairing
    // needs to be stable under.
    facts.defs[1..].sort_by_key(|d| d.span.byte_start);
    facts.refs.sort_by_key(|r| r.span.byte_start);
    facts.header.sources.sort_by_key(|s| s.span.byte_start);
    facts
}

/// `f() { … }`, `function f { … }`, `function f() { … }`.
fn function(facts: &mut FileFacts<BashLang>, node: &SgNode) {
    let Some(name) = node.field("name").map(|n| n.text().to_string()) else {
        return;
    };
    facts.defs.push(Definition {
        kind: DefKind::Function,
        name,
        owner: frames(node),
        space: DeclSpace::Value,
        facets: DefFacets::default(),
        params: None,
        span: span_of(node),
    });
}

/// A command, read only when it is one of the two spellings of `source`.
///
/// Every other command in the file — which is every call site bash has — is
/// deliberately not a reference. See the module docs.
fn command(facts: &mut FileFacts<BashLang>, node: &SgNode) {
    let Some(head) = node.field("name") else {
        return;
    };
    let written = head.text().to_string();
    if !SOURCE_COMMANDS.contains(&written.as_str()) {
        return;
    }
    let args = arg_nodes(node);
    let Some(specifier) = args.first() else {
        return; // `source` with no argument is not an import site
    };
    let span = span_of(node);
    let value = literal(specifier);
    let form = match &value {
        Some(path) => SourceForm::Literal(path.clone()),
        None => SourceForm::Dynamic,
    };
    let target = match &value {
        Some(path) => RefTarget {
            root: TargetRoot::Name,
            segments: vec![path.clone()],
        },
        // The root is not a name: a computed specifier is exactly the shape
        // `TargetRoot::Expr` exists for.
        None => RefTarget {
            root: TargetRoot::Expr,
            segments: Vec::new(),
        },
    };
    facts.header.sources.push(SourceSpec { form, span });
    facts.refs.push(Reference {
        kind: RefKind::Import,
        space: DeclSpace::Namespace,
        // The site as written, specifier included and any further arguments
        // dropped: `source lib/x.bash "$@"` and `source lib/x.bash` name one
        // file, and the extra words are the sourced script's `$@`.
        raw_target: format!("{written} {}", specifier.text()),
        target,
        // Tier 2 emits no expression-level reference, so nothing here can
        // name a local: `LocalBinding` does not apply to this track.
        locally_bound: false,
        argc: None,
        enclosing: enclosing_definition(node),
        span,
    });
}

/// The Bash extractor, as the driver holds it.
pub struct BashExtractor;

impl Extractor<BashLang> for BashExtractor {
    fn extract(&self, rel_path: &str, source: &str) -> FileFacts<BashLang> {
        extract(rel_path, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_compile() {
        Rules::compile(BASH_RULES).expect("bash.yml compiles");
    }

    #[test]
    fn a_call_site_is_not_a_reference() {
        // The load-bearing negative: in bash a call is a `command`, spelled
        // exactly like the builtin this extractor does read. If ordinary
        // commands became references, the rate would measure how much of
        // coreutils lives in the repository.
        let facts = extract(
            "lib/util.bash",
            "printf 'hi'\nbats_trim x\nls | wc -l\nsource lib/a.bash\n",
        );
        assert_eq!(facts.refs.len(), 1, "{:?}", facts.refs);
        assert_eq!(facts.refs[0].raw_target, "source lib/a.bash");
    }

    #[test]
    fn further_arguments_are_the_sourced_scripts_own() {
        let facts = extract("lib/util.bash", "source lib/a.bash --flag \"$@\"\n");
        assert_eq!(facts.refs.len(), 1);
        assert_eq!(facts.refs[0].raw_target, "source lib/a.bash");
        assert_eq!(
            facts.header.sources[0].form,
            SourceForm::Literal("lib/a.bash".to_string()),
        );
    }
}
