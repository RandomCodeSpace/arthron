//! Java's FQN grammar: the one place a Java identity string is built.
//!
//! Both layers that construct an identity read this module — the extractor
//! groups overloads with [`overload_group`], the resolver builds every node
//! name and every candidate here — so a definition's identity and the
//! candidate a reference probes for it cannot drift apart.
//!
//! # The grammar
//!
//! ```text
//! container := Ident ("." Ident)*                com.acme.util        (may be empty: P-03)
//! module    := "module:" Ident ("." Ident)*      module:com.acme.app
//! type      := container "#" Top ("$" Nested)*   com.acme.util#Outer$Inner
//! field     := type "." Ident                    com.acme#Outer$Inner.count
//! method    := type "." Ident "/" argc           com.acme#Outer.doIt/2
//!            | type "." Ident "/*" minarity      com.acme#Outer.format/*1
//!            | type "." Ident "(" Types ")"      com.acme#Outer.doIt(String,int)
//! ctor      := method with Ident = "<init>"      com.acme#Outer.<init>/0
//! ```
//!
//! # Invariants
//!
//! 1. **`#` separates a container from its members, and a container's own name
//!    carries none.** That is the repository's convention, already true of Go's
//!    `{import path}#{Recv}.{name}`. It is what keeps the package
//!    `com.acme.Foo` and the type `Foo` of package `com.acme` two identities —
//!    JLS §6.7's canonical name spells both `com.acme.Foo`, and hashing that
//!    string would merge a package node with a type node.
//! 2. **`.` only joins identifiers within one container**: package segments
//!    before the `#`, and the type-to-member step after it.
//! 3. **`$` only separates type-nesting levels** (§13.1's binary-name rule),
//!    so `com.acme#Outer$Inner` (a nested type) and `com.acme#Outer.Inner` (a
//!    field called `Inner`) are different identities.
//! 4. **No component is occurrence-ordered.** §13.1 numbers anonymous classes
//!    `Outer$1` by order of occurrence; nothing here can name one, because
//!    anonymous and local classes are not definitions (T-03, T-04). Inserting
//!    a declaration anywhere in a file re-keys nothing.
//! 5. **A member key is name plus *arity*, not name plus signature — until the
//!    arity is shared.** See [`member_key`]: this is the one place the grammar
//!    departs from the case study's M-02, and [`overload_group`] is what makes
//!    it injective.
//!
//! # Why arity and not an erased descriptor (a departure from M-02)
//!
//! M-02 proposes `com.acme.Outer#doIt(Ljava/lang/String;[I)` — the erased JVM
//! descriptor. M-09 then observes that building one *is itself resolution*:
//! every parameter type name has to be canonicalized against the compilation
//! unit's scope. Two things make that unbuildable here:
//!
//! * A **reference** never knows the descriptor (M-04). The store a resolver
//!   probes answers "does this identity exist, and what kind is it"; it has no
//!   set-valued probe and the driver never calls
//!   [`crate::lang::Resolver::index_keys`], so the overload-set index M-04 asks
//!   for cannot be built beside the node table. The identity a call site can
//!   construct is therefore the only usable key, and a call site knows exactly
//!   the name and the argument count.
//! * An **edge source** is named by applying the same FQN function to
//!   [`crate::model::Encloser`], which carries no scope and no probe.
//!
//! So a member's key is `name/argc` — or `name/*min` for a variable-arity
//! declaration, whose §15.12.2.4 applicability starts at `min = count − 1`.
//! When one type declares two members that would share a key, neither takes
//! it: both fall back to the **signature form** `name(T1,T2)` spelled with the
//! parameter types *as written*, and the shared key becomes a separate
//! [`crate::model::DefKind::Alias`] node standing for the overload set. A
//! unique callable keeps the arity identity as its node and also emits a
//! signature alias forwarding to that node. Typed applicability can therefore
//! probe one signature grammar for both shapes without re-aiming existing
//! unique-callable edges. M-01's requirement — two overloads are two nodes —
//! holds either way.
//!
//! The signature form is injective within a type because §8.4.8.3 forbids two
//! methods of one class whose erasures are override-equivalent: two members
//! with the same written parameter list cannot both compile.

use crate::model::{DefKind, Definition, Params};

/// Separates a container from its members. Never appears in a container name.
pub const MEMBER: char = '#';

/// Separates type-nesting levels (§13.1).
pub const NEST: char = '$';

/// The name a constructor is a member under. `<` and `>` are excluded from
/// Java identifiers (§3.8), so this can never collide with a method.
pub const INIT: &str = "<init>";

/// The container FQN of a compilation unit: its declared package, or the
/// module it declares.
///
/// P-05 namespaces a module with `module:` so it cannot collide with the
/// package of the same name. P-03's unnamed package is the empty string,
/// which no named package can be.
pub fn container(package: &str, module: Option<&str>) -> String {
    match module {
        Some(name) => format!("module:{name}"),
        None => package.to_string(),
    }
}

/// The FQN of a type declared in `package` at nesting `path`, outermost first.
pub fn type_fqn(package: &str, path: &[String]) -> String {
    format!("{package}{MEMBER}{}", path.join(&NEST.to_string()))
}

/// The FQN of a member of `owner`, which must already be a type FQN.
pub fn member_fqn(owner: &str, key: &str) -> String {
    format!("{owner}.{key}")
}

/// The key a callable is reachable under: name plus arity, or name plus
/// minimum arity when the declaration is variable-arity.
///
/// `/` cannot appear in a Java identifier (§3.8), so a method key can never
/// be read as a field name and `m/2` can never be read as `m`.
pub fn member_key(name: &str, count: u32, varargs: bool) -> String {
    if varargs {
        // §15.12.2.4: a variable-arity method is applicable at `n >= k - 1`.
        varargs_key(name, count.saturating_sub(1))
    } else {
        arity_key(name, count)
    }
}

/// The key of the exact-arity probe a call site with `argc` arguments makes.
pub fn arity_key(name: &str, argc: u32) -> String {
    format!("{name}/{argc}")
}

/// The key of the variable-arity probe for a minimum arity of `min`.
pub fn varargs_key(name: &str, min: u32) -> String {
    format!("{name}/*{min}")
}

/// The signature form used by overload definitions and unique-callable aliases.
pub fn signature_key(name: &str, types: &[String]) -> String {
    format!("{name}({})", types.join(","))
}

/// The overload-group identity a callable competes for, type path included.
///
/// This is what the extractor groups by and what
/// [`crate::track_java::extract::JavaHeader::overloaded`] holds: it names a
/// (type, member name, arity) triple within one compilation unit, which is the
/// whole of what can share a key, because a Java type's members are all
/// declared in one compilation unit.
pub fn overload_group(owner: &[String], name: &str, count: u32, varargs: bool) -> String {
    format!(
        "{}.{}",
        owner.join(&NEST.to_string()),
        member_key(name, count, varargs)
    )
}

/// A callable's member name and parameter shape, however it reached us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callable {
    /// `<init>` for a constructor, the declared name otherwise.
    pub name: String,
    /// Parameter types as written, in order. A variable-arity parameter keeps
    /// its `...`.
    pub types: Vec<String>,
    /// Whether the last parameter is variable-arity.
    pub varargs: bool,
}

impl Callable {
    /// How many parameters are declared.
    pub fn count(&self) -> u32 {
        u32::try_from(self.types.len()).unwrap_or(u32::MAX)
    }

    /// The key this callable takes when nothing else competes for it.
    pub fn key(&self) -> String {
        member_key(&self.name, self.count(), self.varargs)
    }

    /// The key it falls back to when something does.
    pub fn signature(&self) -> String {
        signature_key(&self.name, &self.types)
    }
}

/// Read a callable definition's shape, from either of the two ways one
/// reaches [`crate::lang::Resolver::def_fqn`].
///
/// A real [`Definition`] carries [`Params`]. An [`crate::model::Encloser`]
/// turned back into one by `Encloser::as_definition` carries `params: None`
/// and the parameter list inside its *name* — `m(String,int...)` — because
/// `Encloser::path` is a `Vec<String>` with nowhere else to put it. Both must
/// yield the same shape, or an edge would start at an identity no definition
/// has.
pub fn callable_of(def: &Definition) -> Callable {
    let (name, types) = match &def.params {
        Some(Params { types, .. }) => (def.name.clone(), types.clone()),
        None => split_segment(&def.name),
    };
    let varargs = types.last().is_some_and(|t| t.ends_with("..."));
    let name = if def.kind == DefKind::Constructor {
        INIT.to_string()
    } else {
        name
    };
    Callable {
        name,
        types,
        varargs,
    }
}

/// Split `m(Map<String,Integer>,int...)` into its name and its parameter
/// types.
///
/// Commas inside type arguments are not separators, so the split tracks
/// angle-bracket depth. Java type syntax has no other construct that can
/// carry a top-level comma.
fn split_segment(segment: &str) -> (String, Vec<String>) {
    let Some(open) = segment.find('(') else {
        return (segment.to_string(), Vec::new());
    };
    let name = segment[..open].to_string();
    let inner = segment[open + 1..].strip_suffix(')').unwrap_or("");
    if inner.is_empty() {
        return (name, Vec::new());
    }
    let mut types = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for ch in inner.chars() {
        match ch {
            '<' => {
                depth += 1;
                current.push(ch);
            }
            '>' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => types.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    types.push(current);
    (name, types)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeclSpace, DefFacets, Encloser, Span};

    const NOWHERE: Span = Span {
        byte_start: 0,
        byte_end: 0,
        line: 0,
    };

    fn def(kind: DefKind, name: &str, types: &[&str], varargs: bool) -> Definition {
        let types: Vec<String> = types.iter().map(|t| (*t).to_string()).collect();
        Definition {
            kind,
            name: name.to_string(),
            owner: Vec::new(),
            space: DeclSpace::Value,
            facets: DefFacets::default(),
            params: Some(Params {
                count: u32::try_from(types.len()).unwrap_or(u32::MAX),
                varargs,
                types,
            }),
            span: NOWHERE,
        }
    }

    #[test]
    fn a_container_carries_no_member_separator() {
        assert_eq!(container("com.acme.util", None), "com.acme.util");
        assert!(!container("com.acme.util", None).contains(MEMBER));
        // P-03: the unnamed package is the empty string, which no named
        // package can be.
        assert_eq!(container("", None), "");
        // P-05: a module is namespaced so it cannot collide with a package.
        assert_eq!(
            container("com.acme.app", Some("com.acme.app")),
            "module:com.acme.app"
        );
    }

    #[test]
    fn a_package_and_a_type_of_the_same_canonical_name_are_two_identities() {
        // JLS §6.7 spells both `com.acme.Foo`. Hashing that string would give
        // the package node and the type node one identity.
        let package = container("com.acme.Foo", None);
        let ty = type_fqn("com.acme", &["Foo".to_string()]);
        assert_ne!(package, ty);
        assert_eq!(ty, "com.acme#Foo");
    }

    #[test]
    fn nesting_uses_dollar_and_membership_uses_dot() {
        let outer = type_fqn("com.acme", &["Outer".to_string()]);
        let inner = type_fqn("com.acme", &["Outer".to_string(), "Inner".to_string()]);
        assert_eq!(inner, "com.acme#Outer$Inner");
        // A field called `Inner` is not the nested type `Inner`.
        assert_ne!(member_fqn(&outer, "Inner"), inner);
        assert_eq!(member_fqn(&outer, "Inner"), "com.acme#Outer.Inner");
        // Exactly one `#` in a definition's FQN, and none in a container's.
        assert_eq!(inner.matches(MEMBER).count(), 1);
    }

    #[test]
    fn a_member_key_is_name_and_arity() {
        assert_eq!(member_key("doIt", 2, false), "doIt/2");
        assert_eq!(arity_key("doIt", 2), "doIt/2");
        // §15.12.2.4: `f(String, Object...)` has k = 2 and applies from n = 1.
        assert_eq!(member_key("format", 2, true), "format/*1");
        assert_eq!(varargs_key("format", 1), "format/*1");
        // `f(Object...)` applies from n = 0 and must not underflow.
        assert_eq!(member_key("of", 1, true), "of/*0");
        // A field and a no-argument method of one name are two identities.
        assert_ne!(member_key("count", 0, false), "count");
    }

    #[test]
    fn an_overload_group_names_a_type_a_name_and_an_arity() {
        let owner = vec!["Outer".to_string(), "Inner".to_string()];
        assert_eq!(overload_group(&owner, "m", 2, false), "Outer$Inner.m/2");
        assert_ne!(
            overload_group(&owner, "m", 2, false),
            overload_group(&owner, "m", 1, false)
        );
        // A fixed-arity and a variable-arity declaration of the same count do
        // not compete: §15.12.2's phases 1 and 2 exclude varargs entirely, so
        // the fixed one takes every call it is applicable to.
        assert_ne!(
            overload_group(&owner, "m", 2, false),
            overload_group(&owner, "m", 2, true)
        );
    }

    #[test]
    fn a_callable_reads_the_same_from_a_definition_and_from_an_encloser() {
        // The two ways `def_fqn` is reached must agree, or an edge would start
        // at an identity no definition has.
        let declared = def(
            DefKind::Method,
            "m",
            &["Map<String,Integer>", "int..."],
            true,
        );
        let from_def = callable_of(&declared);
        let encloser = Encloser {
            path: vec!["A".to_string(), "m(Map<String,Integer>,int...)".to_string()],
            kind: DefKind::Method,
        };
        let from_encloser = callable_of(&encloser.as_definition().expect("nameable"));
        assert_eq!(from_def, from_encloser);
        // The comma inside the type arguments is not a separator.
        assert_eq!(from_def.types, ["Map<String,Integer>", "int..."]);
        assert_eq!(from_def.count(), 2);
        assert!(from_def.varargs);
        assert_eq!(from_def.key(), "m/*1");
        assert_eq!(from_def.signature(), "m(Map<String,Integer>,int...)");
    }

    #[test]
    fn a_constructor_is_a_member_named_init() {
        let declared = def(DefKind::Constructor, "A", &["int"], false);
        assert_eq!(callable_of(&declared).name, INIT);
        let encloser = Encloser {
            path: vec!["A".to_string(), "A(int)".to_string()],
            kind: DefKind::Constructor,
        };
        let from_encloser = callable_of(&encloser.as_definition().expect("nameable"));
        assert_eq!(from_encloser, callable_of(&declared));
        assert_eq!(from_encloser.key(), "<init>/1");
    }

    #[test]
    fn a_no_argument_callable_round_trips() {
        let declared = def(DefKind::Method, "run", &[], false);
        let encloser = Encloser {
            path: vec!["A".to_string(), "run()".to_string()],
            kind: DefKind::Method,
        };
        assert_eq!(
            callable_of(&declared),
            callable_of(&encloser.as_definition().expect("nameable"))
        );
        assert_eq!(callable_of(&declared).key(), "run/0");
    }
}
