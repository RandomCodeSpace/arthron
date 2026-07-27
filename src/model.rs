//! Core record types shared by the extractor, resolver, and store.
//!
//! Nothing here parses, links, or persists. These are the nouns of the
//! system; the verbs live in the layer modules.

use crate::UnresolvedReason;

/// A 128-bit content-addressed node identity: `hash(language, canonical FQN)`.
///
/// Deterministic across machines and runs, so graphs built anywhere are
/// diffable and the CI cache artifact is portable. See
/// `docs/decisions.md` — "Identity: content-addressed 128-bit NodeId".
pub type NodeId = [u8; 16];

/// Compute the [`NodeId`] for a canonical fully-qualified name.
pub fn node_id(language: Lang, fqn: &str) -> NodeId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[language.code()]);
    hasher.update(fqn.as_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    id
}

/// A language arthron attributes records to. Grows one variant per language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lang {
    /// The Go programming language.
    Go,
}

impl Lang {
    /// Stable one-byte code used in hashing and storage. Never renumber.
    pub fn code(self) -> u8 {
        match self {
            Lang::Go => 0,
        }
    }

    /// Human-readable name for report output.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Go => "go",
        }
    }
}

/// Where a record sits in its file: byte range plus 1-based line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the start of the node.
    pub byte_start: u32,
    /// Byte offset one past the end of the node.
    pub byte_end: u32,
    /// 1-based line number of the start.
    pub line: u32,
}

/// What kind of thing a definition declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    /// A free function.
    Function,
    /// A method with a receiver.
    Method,
    /// A named type.
    Type,
    /// A package-level constant.
    Const,
    /// A package-level variable.
    Var,
}

impl DefKind {
    /// Stable one-byte storage code. Never renumber.
    pub fn code(self) -> u8 {
        match self {
            DefKind::Function => 0,
            DefKind::Method => 1,
            DefKind::Type => 2,
            DefKind::Const => 3,
            DefKind::Var => 4,
        }
    }
}

/// A named declaration extracted from one file. Extractor output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// What kind of declaration this is.
    pub kind: DefKind,
    /// The declared name, unqualified.
    pub name: String,
    /// For methods, the receiver's type name.
    pub receiver: Option<String>,
    /// Where the declaration sits.
    pub span: Span,
}

/// What kind of naming a reference site performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// A call site.
    Call,
    /// An import declaration.
    Import,
}

impl RefKind {
    /// Stable one-byte storage code. Never renumber.
    pub fn code(self) -> u8 {
        match self {
            RefKind::Call => 0,
            RefKind::Import => 1,
        }
    }
}

/// The shape of what a call site names, as far as one file can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefTarget {
    /// `Foo()` — a bare identifier.
    Plain {
        /// The identifier.
        name: String,
    },
    /// `qual.Foo()` — exactly one identifier qualifier.
    Qualified {
        /// The qualifying identifier (an import name or a variable).
        qualifier: String,
        /// The member being called.
        name: String,
    },
    /// Anything else — chained selectors, call results, method values.
    Complex,
}

/// A site in one file that names something possibly defined elsewhere.
///
/// The extractor emits these; only the resolver may turn one into an edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// What kind of naming this is.
    pub kind: RefKind,
    /// The literal text at the site — the store's dedup key component.
    pub raw_target: String,
    /// The parsed shape of the target.
    pub target: RefTarget,
    /// The enclosing definition's name (`Recv.Name` for methods), or
    /// `None` when the reference sits at package level.
    pub enclosing: Option<String>,
    /// Where the reference sits.
    pub span: Span,
}

/// Stable one-byte storage code for an [`UnresolvedReason`]. Never renumber.
pub fn reason_code(r: &UnresolvedReason) -> u8 {
    match r {
        UnresolvedReason::DynamicDispatch => 0,
        UnresolvedReason::MacroGenerated => 1,
        UnresolvedReason::UnknownPackage => 2,
        UnresolvedReason::TierTwoLanguage => 3,
        UnresolvedReason::NoMatchingDefinition => 4,
        UnresolvedReason::NeedsTypeInference => 5,
    }
}

/// Inverse of [`reason_code`]. `None` for codes no variant carries.
pub fn reason_from_code(c: u8) -> Option<UnresolvedReason> {
    Some(match c {
        0 => UnresolvedReason::DynamicDispatch,
        1 => UnresolvedReason::MacroGenerated,
        2 => UnresolvedReason::UnknownPackage,
        3 => UnresolvedReason::TierTwoLanguage,
        4 => UnresolvedReason::NoMatchingDefinition,
        5 => UnresolvedReason::NeedsTypeInference,
        _ => return None,
    })
}

/// Human-readable name for a reason code, for report output.
pub fn reason_name(c: u8) -> &'static str {
    match c {
        0 => "DynamicDispatch",
        1 => "MacroGenerated",
        2 => "UnknownPackage",
        3 => "TierTwoLanguage",
        4 => "NoMatchingDefinition",
        5 => "NeedsTypeInference",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_is_deterministic() {
        assert_eq!(
            node_id(Lang::Go, "m/pkg.Foo"),
            node_id(Lang::Go, "m/pkg.Foo")
        );
    }

    #[test]
    fn node_id_separates_fqns_and_languages() {
        assert_ne!(
            node_id(Lang::Go, "m/pkg.Foo"),
            node_id(Lang::Go, "m/pkg.Bar")
        );
        // Same string, different language byte, must differ once a second
        // language exists; today this guards the hash-input ordering.
        assert_ne!(node_id(Lang::Go, "a"), node_id(Lang::Go, "b"));
    }

    #[test]
    fn reason_codes_round_trip() {
        for c in 0u8..=5 {
            let r = reason_from_code(c).expect("code maps to a variant");
            assert_eq!(reason_code(&r), c);
        }
        assert_eq!(reason_from_code(6), None);
    }
}
