//! The query surface: a name in, the graph's own answer out.
//!
//! This layer resolves nothing and links nothing. Every fact it returns was
//! decided by a resolver and written by [`crate::store`]; the work here is
//! turning a name a person can type into the identities the graph already
//! holds, and then reading what it says about them.
//!
//! # Names
//!
//! A [`NodeId`] is `hash(domain, canonical FQN)`, and a hash cannot be turned
//! back into a name. So a lookup goes the other way: [`NameIndex`] reads every
//! node's stored name once and matches against those. Two spellings are
//! accepted, and the order between them is the whole rule:
//!
//! 1. **An exact FQN wins outright.** `example.com/app/util#Parse` selects
//!    that node and is never widened, even when it is also the suffix of a
//!    longer name.
//! 2. **Otherwise, a suffix at a separator.** `Parse` selects every node
//!    whose name ends in `Parse` at a boundary an identifier cannot cross.
//!
//! Two matches are an *answer*, not an error. Every FQN grammar in this
//! repository is per-domain injective, so a bare `Parse` genuinely can name
//! two definitions, and picking one of them would be a guess — the same guess
//! the resolver is forbidden from making. The caller is handed both.
//!
//! # What the three verbs read
//!
//! - [`definition`] reads the node table: the record, and every site that
//!   declares it. A node two files declare has two sites, and both are here.
//! - [`references`] reads the reference rows: every row the resolver stored
//!   with an outcome of `Resolved(id)`.
//! - [`impact`] walks the reverse-edge index outward from the node, layer by
//!   layer, cycle-guarded and depth-bounded, and says when the bound cut the
//!   walk short rather than presenting a truncated answer as a complete one.

use std::collections::HashSet;

use crate::model::{DefKind, Lang, NodeId, RefKind};
use crate::store::{DeclSite, NodeRecord, ReadStore, RefKey, RefRecord, StoredOutcome};

/// How many hops of the reverse closure [`impact`] walks when the caller does
/// not say.
///
/// Three, because the reverse closure of a widely-used definition grows by
/// roughly a fan-in factor per hop and the fourth layer is routinely most of
/// the repository — an answer nobody reads. It is a display bound, not a
/// claim about the graph: [`Impact::truncated`] says when it cut something.
pub const DEFAULT_IMPACT_DEPTH: u32 = 3;

/// What a stored node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// A definition inside this repository, by its declared kind.
    Definition(DefKind),
    /// A package or module inside this repository.
    Package,
    /// A dependency outside this repository.
    External,
    /// An identity an edge names that the node table does not hold.
    ///
    /// Reported rather than skipped. A dangling edge would be a bug in the
    /// store's file ownership, and a query that quietly dropped the row it
    /// points at is exactly how such a bug stays invisible.
    Missing,
}

/// One node the graph holds, under the name it answers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The node's identity.
    pub id: NodeId,
    /// Its stored name: a definition's canonical FQN, a package's import
    /// path, or an external dependency's package string. For
    /// [`NodeKind::Missing`] there is no stored name, and this is the
    /// identity in hex.
    pub name: String,
    /// What kind of node it is.
    pub kind: NodeKind,
}

/// A node's own record, and every site that declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// Identity, name and kind.
    pub node: Match,
    /// Every declaration site, sorted by `(file, line)` as the store keeps
    /// them. More than one is not a contradiction: a build-tag twin pair
    /// declares one identity from two files, and both sites are facts.
    pub declarations: Vec<DeclSite>,
    /// What this identity forwards to, when it is an alias. Empty for every
    /// ordinary definition.
    pub targets: Vec<Match>,
}

/// One stored reference row, as a query reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefSite {
    /// Repo-relative path of the file the reference sits in.
    pub file: String,
    /// 1-based line of the row's first occurrence.
    pub line: u32,
    /// What the site does, or `None` for a stored code no variant carries.
    pub kind: Option<RefKind>,
    /// The FQN of the definition the site sits in — the edge's source.
    pub enclosing: String,
    /// The literal text at the site.
    pub raw_target: String,
    /// How many times this row occurs in its file.
    pub count: u32,
    /// The outcome the resolver stored for it.
    pub outcome: StoredOutcome,
    /// The language the row is tallied under, or `None` for a stored code no
    /// variant carries.
    pub lang: Option<Lang>,
}

/// The reverse closure of a node, layer by layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Impact {
    /// One list per hop, nearest first: `layers[0]` is everything one edge
    /// from the node. A node appears in the shallowest layer that reaches it
    /// and never again, so the layers partition the closure.
    pub layers: Vec<Vec<Match>>,
    /// Whether the depth bound cut the walk with more still to find.
    ///
    /// Stated rather than left to the reader, because a bounded closure and
    /// an exhausted one look identical and only one of them is the answer to
    /// "what reaches this".
    pub truncated: bool,
}

/// Every node name in the graph, for name lookup.
///
/// Built once and queried many times: the build reads the whole node table,
/// so a caller asking about three names should build one index, not three.
pub struct NameIndex {
    /// Every node, sorted by `(name, id)` so a lookup's answer is stable
    /// across runs.
    entries: Vec<Match>,
}

impl NameIndex {
    /// Read every node's name and kind out of the store.
    pub fn build(store: &ReadStore) -> Result<Self, String> {
        let mut entries = Vec::new();
        store.for_each_node(|id, record| {
            entries.push(to_match(id, &record)?);
            Ok(())
        })?;
        entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(NameIndex { entries })
    }

    /// How many nodes the index holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the graph holds no nodes at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every node this name selects, in stable order.
    ///
    /// An exact name match wins outright and is never widened into a suffix
    /// search. With no exact match, every node whose name ends in the query
    /// at a separator boundary is returned — none, one, or several. See the
    /// module header for why several is an answer.
    pub fn lookup(&self, name: &str) -> Vec<Match> {
        if name.is_empty() {
            return Vec::new();
        }
        let exact: Vec<Match> = self
            .entries
            .iter()
            .filter(|entry| entry.name == name)
            .cloned()
            .collect();
        if !exact.is_empty() {
            return exact;
        }
        self.entries
            .iter()
            .filter(|entry| ends_at_separator(&entry.name, name))
            .cloned()
            .collect()
    }
}

/// Whether `name` ends with `suffix` without cutting an identifier in half.
///
/// One rule for every FQN grammar in the repository, because they agree on the
/// part that matters: an identifier is made of alphanumerics and `_`, and every
/// separator any of them uses — `.`, `#`, `/`, `$`, `:`, `!`, `(`, `,`, `)` —
/// is neither. So the test is not "the query starts at a separator" but the
/// weaker and correct one: **the cut does not fall inside an identifier**.
///
/// The difference is `#Parse` against `example.com/app/util#Parse`. The
/// character before the cut is `l`, an identifier character, and the stricter
/// rule would reject a query that plainly names the node. What actually
/// matters is that the character before the cut and the first character of the
/// query are not *both* identifier characters — which is exactly what it means
/// for the cut to split a name. `arse` is rejected by it; `#Parse`, `Inner` in
/// `com.acme#Outer$Inner`, and `C.m` in `pkg.sub#C.m` are not.
///
/// `is_alphanumeric` is Unicode-aware, so an identifier spelled outside ASCII
/// is one identifier here too.
fn ends_at_separator(name: &str, suffix: &str) -> bool {
    if name.len() <= suffix.len() || !name.ends_with(suffix) {
        return false;
    }
    let cut = name.len() - suffix.len();
    // A cut landing mid-codepoint is not a suffix of any name a person typed;
    // `ends_with` already ruled it out, so this only guards the slice.
    if !name.is_char_boundary(cut) {
        return false;
    }
    let before = name[..cut].chars().next_back();
    let first = suffix.chars().next();
    // `name` is longer than `suffix` and `suffix` is non-empty at every call
    // site, so both are `Some`; a `None` cannot split an identifier either.
    !(before.is_some_and(is_ident) && first.is_some_and(is_ident))
}

/// Whether a character can sit inside an identifier in any language here.
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The definition record for an identity, with every site that declares it.
///
/// `None` when the graph holds no node under this identity — a fact, not a
/// failure: the name may be spelled for a definition this scan never saw.
pub fn definition(store: &ReadStore, id: &NodeId) -> Result<Option<Definition>, String> {
    let Some(record) = store.node(id)? else {
        return Ok(None);
    };
    let node = to_match(*id, &record)?;
    let declarations = record.declarations().to_vec();
    let mut targets = Vec::new();
    if let NodeRecord::Definition { targets: ids, .. } = &record {
        for target in ids {
            targets.push(match store.node(target)? {
                Some(found) => to_match(*target, &found)?,
                // An alias pointing at nothing is reported as such. The
                // alternative — omitting it — would make a broken forward
                // read as an alias with fewer targets than it has.
                None => missing(*target),
            });
        }
    }
    Ok(Some(Definition {
        node,
        declarations,
        targets,
    }))
}

/// Every stored reference row whose outcome resolved to this identity.
///
/// Ordered by `(file, line, enclosing, raw_target)`, so two runs over one
/// store print the same list.
pub fn references(store: &ReadStore, id: &NodeId) -> Result<Vec<RefSite>, String> {
    let mut out = Vec::new();
    store.for_each_row(|key, record| {
        if record.outcome == StoredOutcome::Resolved(*id) {
            out.push(to_site(key, record));
        }
        Ok(())
    })?;
    out.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.enclosing.cmp(&b.enclosing))
            .then_with(|| a.raw_target.cmp(&b.raw_target))
    });
    Ok(out)
}

/// The transitive reverse closure of an identity, up to `depth` hops.
///
/// Breadth-first over the reverse-edge index, with a visited set that both
/// terminates cycles and keeps every node in the shallowest layer that
/// reaches it. The node asked about is seeded as visited, so a definition
/// that reaches itself never appears in its own impact.
///
/// `depth` of zero walks nothing and still reports whether there was
/// something to walk.
pub fn impact(store: &ReadStore, id: &NodeId, depth: u32) -> Result<Impact, String> {
    let mut seen: HashSet<NodeId> = HashSet::from([*id]);
    let mut frontier: Vec<NodeId> = vec![*id];
    let mut layers: Vec<Vec<Match>> = Vec::new();

    for _ in 0..depth {
        let next = predecessors(store, &frontier, &mut seen)?;
        if next.is_empty() {
            frontier.clear();
            break;
        }
        let mut layer = Vec::with_capacity(next.len());
        for node in &next {
            layer.push(named(store, *node)?);
        }
        layer.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        layers.push(layer);
        frontier = next;
    }

    // One probe past the bound, and only to answer the question the bound
    // raises. Nothing it finds is added to the answer: the point is to
    // distinguish a closure that ended from one that was cut, which the
    // layers alone cannot express.
    let truncated = !frontier.is_empty() && !predecessors(store, &frontier, &mut seen)?.is_empty();
    Ok(Impact { layers, truncated })
}

/// Every unvisited node with an edge into any of `frontier`, marking each
/// visited as it goes.
fn predecessors(
    store: &ReadStore,
    frontier: &[NodeId],
    seen: &mut HashSet<NodeId>,
) -> Result<Vec<NodeId>, String> {
    let mut next = Vec::new();
    for node in frontier {
        for (src, _kind) in store.edges_into(node)? {
            if seen.insert(src) {
                next.push(src);
            }
        }
    }
    Ok(next)
}

/// The name and kind stored for an identity, or a [`NodeKind::Missing`]
/// placeholder when the node table does not hold it.
fn named(store: &ReadStore, id: NodeId) -> Result<Match, String> {
    match store.node(&id)? {
        Some(record) => to_match(id, &record),
        None => Ok(missing(id)),
    }
}

/// A [`Match`] for an identity no node record backs.
fn missing(id: NodeId) -> Match {
    use std::fmt::Write as _;
    let mut name = String::with_capacity(id.len() * 2);
    for byte in id {
        // Writing into a `String` cannot fail; the `Result` is the trait's.
        let _ = write!(name, "{byte:02x}");
    }
    Match {
        id,
        name,
        kind: NodeKind::Missing,
    }
}

/// The name and kind a stored record answers to.
fn to_match(id: NodeId, record: &NodeRecord) -> Result<Match, String> {
    let (name, kind) = match record {
        NodeRecord::Definition { fqn, kind, .. } => (
            fqn.clone(),
            NodeKind::Definition(
                DefKind::from_code(*kind)
                    .ok_or_else(|| format!("stored node kind {kind} has no variant"))?,
            ),
        ),
        NodeRecord::Package { import_path, .. } => (import_path.clone(), NodeKind::Package),
        NodeRecord::External { package, .. } => (package.clone(), NodeKind::External),
    };
    Ok(Match { id, name, kind })
}

/// Flatten one stored row into the site a query reports.
fn to_site(key: RefKey, record: RefRecord) -> RefSite {
    RefSite {
        file: key.file,
        line: record.first_line,
        kind: RefKind::from_code(key.kind),
        enclosing: key.enclosing,
        raw_target: key.raw_target,
        count: record.count,
        outcome: record.outcome,
        lang: Lang::from_code(record.lang),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suffix_may_not_cut_an_identifier_in_half() {
        assert!(ends_at_separator("example.com/app/util#Parse", "Parse"));
        assert!(ends_at_separator("com.acme#Outer$Inner", "Inner"));
        assert!(ends_at_separator("pkg.sub#C.m", "C.m"));
        assert!(!ends_at_separator("example.com/app/util#Parse", "arse"));
        assert!(
            !ends_at_separator("Parse", "Parse"),
            "equality is not a suffix"
        );
        assert!(!ends_at_separator("Parse", "LongerThanTheName"));
    }

    #[test]
    fn a_query_that_opens_with_a_separator_needs_no_separator_before_it() {
        // The character before the cut is `l` here, and the query still names
        // the node exactly. Only a cut with an identifier character on both
        // sides splits a name.
        assert!(ends_at_separator("example.com/app/util#Parse", "#Parse"));
        assert!(ends_at_separator(
            "example.com/app/util#Parse",
            "/util#Parse"
        ));
        assert!(ends_at_separator("pkg.sub#C.m", ".m"));
        // …and it is not a licence to start mid-identifier: `l#Parse` cuts
        // `util`, both sides of the cut are identifier characters, and it is
        // still rejected.
        assert!(!ends_at_separator("example.com/app/util#Parse", "l#Parse"));
    }

    #[test]
    fn a_separator_is_anything_an_identifier_is_not() {
        // Every grammar in the repository, one rule.
        assert!(ends_at_separator("a/b#c", "c"));
        assert!(ends_at_separator("com.acme#Outer.doIt/2", "2"));
        assert!(ends_at_separator("com.acme#Outer.doIt(String,int)", "int)"));
        assert!(ends_at_separator("external:npm:fastify", "fastify"));
        // `_` joins an identifier, so it is not a boundary.
        assert!(!ends_at_separator("pkg#do_parse", "parse"));
    }

    #[test]
    fn a_non_ascii_identifier_is_still_one_identifier() {
        // `is_alphanumeric` is Unicode-aware, so a Go or Python identifier
        // spelled outside ASCII does not accidentally become a boundary.
        assert!(!ends_at_separator("pkg#naïveParse", "Parse"));
        assert!(ends_at_separator("pkg#naïve", "naïve"));
    }

    #[test]
    fn a_missing_identity_renders_as_its_hex() {
        let id = [0x00, 0xff, 0x10, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        let m = missing(id);
        assert_eq!(m.id, id);
        assert_eq!(m.kind, NodeKind::Missing);
        assert_eq!(m.name, "00ff100102030405060708090a0b0c0d");
    }
}
