//! Core record types shared by the extractor, resolver, and store.
//!
//! Nothing here parses, links, or persists. These are the nouns of the
//! system; the verbs live in the layer modules.

use crate::UnresolvedReason;

/// A 128-bit content-addressed node identity: `hash(domain, canonical FQN)`.
///
/// Deterministic across machines and runs, so graphs built anywhere are
/// diffable and the CI cache artifact is portable. See
/// `docs/decisions.md` — "Identity: content-addressed 128-bit NodeId".
pub type NodeId = [u8; 16];

/// The identity space a node's id is hashed in.
///
/// A *domain* is wider than a language: sibling languages that name each
/// other's definitions share one. A `.ts` file naming a definition in a
/// `.js` file must probe an identity that can exist, which it cannot when
/// the language is the hash input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Domain {
    /// Go.
    Go = 0,
    /// The JVM languages, Java first.
    Jvm = 1,
    /// The ECMAScript family: JavaScript, TypeScript and every dialect
    /// (`.mjs`, `.cjs`, `.jsx`, `.tsx`, `.d.ts`).
    EcmaScript = 2,
    /// Python.
    Python = 3,
    // The ratified tier-2 languages, registered ahead of their tracks. Each
    // takes its **own** identity space. A domain is shared only where one
    // language's resolver has to find another language's declarations, and no
    // tier-2 resolver exists yet to do so — so Kotlin and Scala are
    // deliberately *not* folded into `Jvm` here. Putting them there would
    // commit Java's FQN grammar to answering for their declarations, which is
    // a decision for whichever of those tracks lands second, not for a
    // registration that reads no files.
    /// C++ — and C, when it lands on the same track. Named for the family
    /// rather than the language because a `.h` header is named by both, so
    /// one identity space is the only arrangement in which each can find the
    /// other's declarations.
    Cxx = 4,
    /// C#.
    CSharp = 5,
    /// Kotlin.
    Kotlin = 6,
    /// Swift.
    Swift = 7,
    /// Ruby.
    Ruby = 8,
    /// PHP.
    Php = 9,
    /// Rust.
    Rust = 10,
    /// Scala.
    Scala = 11,
    /// Dart.
    Dart = 12,
    /// Elixir.
    Elixir = 13,
    /// Haskell.
    Haskell = 14,
    /// Lua.
    Lua = 15,
    /// The shell family, Bash first.
    Shell = 16,
    /// HCL, the HashiCorp configuration language.
    Hcl = 17,
}

impl Domain {
    /// Stable one-byte code used in hashing and storage. Never renumber.
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// Compute the [`NodeId`] for a canonical fully-qualified name in a domain.
pub fn node_id(domain: Domain, fqn: &str) -> NodeId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[domain.code()]);
    hasher.update(fqn.as_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    id
}

/// A language arthron attributes records to. Grows one variant per language.
///
/// A *language* is not a [`Domain`] and not a track. It is the unit a
/// resolution rate is reported under, and rates are never aggregated — which
/// is why [`Lang::JavaScript`] and [`Lang::TypeScript`] are two variants even
/// though they share [`Domain::EcmaScript`] and one resolver family. One
/// combined EcmaScript number would let a collapse in one of them be masked
/// by the other.
///
/// Codes 0–4 are the committed language order and are storage bytes: `Go = 0`,
/// `Java = 1`, `JavaScript = 2`, `TypeScript = 3`, `Python = 4`. Appending is
/// the only permitted change; nothing here is ever renumbered. Codes 5–18 are
/// the ratified tier-2 languages, appended in their ratified order and
/// registered ahead of the tracks that will implement them — a variant here
/// is a name and a storage byte, not a claim that anything scans it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lang {
    /// The Go programming language.
    Go,
    /// The Java programming language.
    Java,
    /// JavaScript, including the `.mjs` and `.cjs` module dialects.
    JavaScript,
    /// TypeScript. A `.d.ts` declaration file is a `.ts` file — the extension
    /// is `ts` and the `.d` is part of the stem — so it needs no ownership
    /// rule of its own.
    TypeScript,
    /// The Python programming language.
    Python,
    // The ratified tier-2 languages: the full tier-2 set first (C++ through
    // Scala), then the best-effort set (Dart through HCL). Every one of them
    // has a grammar in the pinned ast-grep build; a language without one is
    // not registered here. They are listed now so that bringing one up edits
    // only that track's own module — see [`crate::registry`].
    /// C++. C is not this variant; it rides the same track later.
    Cpp,
    /// C#.
    CSharp,
    /// Kotlin.
    Kotlin,
    /// Swift.
    Swift,
    /// Ruby.
    Ruby,
    /// PHP.
    Php,
    /// The Rust programming language.
    Rust,
    /// Scala.
    Scala,
    /// Dart.
    Dart,
    /// Elixir.
    Elixir,
    /// Haskell.
    Haskell,
    /// Lua.
    Lua,
    /// Bash.
    Bash,
    /// HCL, the HashiCorp configuration language.
    Hcl,
}

impl Lang {
    /// Every language, in committed code order. The registry and the
    /// extension lookup both walk this, so a variant that is not listed here
    /// owns nothing.
    pub const ALL: &'static [Lang] = &[
        Lang::Go,
        Lang::Java,
        Lang::JavaScript,
        Lang::TypeScript,
        Lang::Python,
        Lang::Cpp,
        Lang::CSharp,
        Lang::Kotlin,
        Lang::Swift,
        Lang::Ruby,
        Lang::Php,
        Lang::Rust,
        Lang::Scala,
        Lang::Dart,
        Lang::Elixir,
        Lang::Haskell,
        Lang::Lua,
        Lang::Bash,
        Lang::Hcl,
    ];

    /// Stable one-byte code used in reporting and storage. Never renumber.
    pub fn code(self) -> u8 {
        match self {
            Lang::Go => 0,
            Lang::Java => 1,
            Lang::JavaScript => 2,
            Lang::TypeScript => 3,
            Lang::Python => 4,
            Lang::Cpp => 5,
            Lang::CSharp => 6,
            Lang::Kotlin => 7,
            Lang::Swift => 8,
            Lang::Ruby => 9,
            Lang::Php => 10,
            Lang::Rust => 11,
            Lang::Scala => 12,
            Lang::Dart => 13,
            Lang::Elixir => 14,
            Lang::Haskell => 15,
            Lang::Lua => 16,
            Lang::Bash => 17,
            Lang::Hcl => 18,
        }
    }

    /// Human-readable name for report output.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Go => "go",
            Lang::Java => "java",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Python => "python",
            Lang::Cpp => "cpp",
            Lang::CSharp => "csharp",
            Lang::Kotlin => "kotlin",
            Lang::Swift => "swift",
            Lang::Ruby => "ruby",
            Lang::Php => "php",
            Lang::Rust => "rust",
            Lang::Scala => "scala",
            Lang::Dart => "dart",
            Lang::Elixir => "elixir",
            Lang::Haskell => "haskell",
            Lang::Lua => "lua",
            Lang::Bash => "bash",
            Lang::Hcl => "hcl",
        }
    }

    /// The capability tier this language's track reports at.
    ///
    /// Tier 1 resolves the call graph, so its rate is taken over calls, type
    /// uses and imports together. Tier 2 extracts definitions, structure and
    /// imports and emits no call reference at all, so its rate is an
    /// *import*-resolution rate over a strictly smaller denominator — the
    /// shape ratified in `docs/decisions.md`, 2026-07-27.
    ///
    /// Both are per-language and neither is ever aggregated, but two rates
    /// printed in one column are read as one measurement unless something
    /// says otherwise. This is that something: it is what the report line and
    /// the `scan --json` tally state, so a tier-2 85% is never mistaken for a
    /// tier-1 85%.
    pub fn tier(self) -> u8 {
        match self {
            Lang::Go | Lang::Java | Lang::JavaScript | Lang::TypeScript | Lang::Python => 1,
            _ => 2,
        }
    }

    /// What this language's resolution rate is a rate *of*, in words, for the
    /// human-readable report. Derived from [`Lang::tier`] so the number and
    /// the phrase cannot disagree.
    pub fn rate_scope(self) -> &'static str {
        match self.tier() {
            1 => "call-graph resolution",
            _ => "import resolution",
        }
    }

    /// Inverse of [`Lang::code`]. `None` for codes no variant carries.
    pub fn from_code(c: u8) -> Option<Lang> {
        match c {
            0 => Some(Lang::Go),
            1 => Some(Lang::Java),
            2 => Some(Lang::JavaScript),
            3 => Some(Lang::TypeScript),
            4 => Some(Lang::Python),
            5 => Some(Lang::Cpp),
            6 => Some(Lang::CSharp),
            7 => Some(Lang::Kotlin),
            8 => Some(Lang::Swift),
            9 => Some(Lang::Ruby),
            10 => Some(Lang::Php),
            11 => Some(Lang::Rust),
            12 => Some(Lang::Scala),
            13 => Some(Lang::Dart),
            14 => Some(Lang::Elixir),
            15 => Some(Lang::Haskell),
            16 => Some(Lang::Lua),
            17 => Some(Lang::Bash),
            18 => Some(Lang::Hcl),
            _ => None,
        }
    }

    /// The identity space this language's nodes are hashed in.
    pub fn domain(self) -> Domain {
        match self {
            Lang::Go => Domain::Go,
            Lang::Java => Domain::Jvm,
            Lang::JavaScript | Lang::TypeScript => Domain::EcmaScript,
            Lang::Python => Domain::Python,
            Lang::Cpp => Domain::Cxx,
            Lang::CSharp => Domain::CSharp,
            Lang::Kotlin => Domain::Kotlin,
            Lang::Swift => Domain::Swift,
            Lang::Ruby => Domain::Ruby,
            Lang::Php => Domain::Php,
            Lang::Rust => Domain::Rust,
            Lang::Scala => Domain::Scala,
            Lang::Dart => Domain::Dart,
            Lang::Elixir => Domain::Elixir,
            Lang::Haskell => Domain::Haskell,
            Lang::Lua => Domain::Lua,
            Lang::Bash => Domain::Shell,
            Lang::Hcl => Domain::Hcl,
        }
    }

    /// File extensions this language owns, without the dot.
    ///
    /// Ownership is a partition: exactly one language owns any extension, so
    /// a walk never has to ask which of two claimants meant it. That is what
    /// keeps [`Lang::for_extension`] a function rather than a policy.
    ///
    /// A language's claim here is committed ahead of its track. While the
    /// track is disabled it owns no file at all
    /// ([`crate::registry::Track::owns_extension`]), so a tier-2 entry
    /// changes nothing a scan reads; the go-live wave finalizes the list.
    ///
    /// The partition is committed; the tier-2 lists are not yet complete.
    /// Each holds a minimal claim — `.hcl` and `.tfvars`, `.sbt`, `.zsh` and
    /// the like are left unclaimed rather than assigned elsewhere — because
    /// widening a disabled language's list changes nothing a scan reads, and
    /// that track's go-live commit is the first point at which anyone has
    /// parsed the files and can say the claim holds.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Lang::Go => &["go"],
            Lang::Java => &["java"],
            Lang::JavaScript => &["js", "mjs", "cjs"],
            Lang::TypeScript => &["ts"],
            Lang::Python => &["py"],
            // Deliberately **not** `c`. Measured against the pinned
            // grammar: a K&R-style definition — valid C, not valid C++ —
            // parses cleanly under the C grammar and yields an error node
            // under the C++ one, so the two really are different languages
            // here, a `.c` file read on this track is read as the wrong
            // one, and `.c` waits for C's own decision. `.h` carried the
            // same hazard — it is the C header extension before it is
            // anything else — but the C++ track claims it regardless,
            // because header-dominated C++ libraries are unmeasurable
            // without it: fmt keeps 21 of its 55 corpus files in `.h`, and
            // the track measured 3.4% import resolution refusing them
            // against 87.6% reading them. The accepted risk is that a
            // pure-C repository's `.h` files parse under the C++ grammar;
            // it stands until a C track exists and arbitrates ownership —
            // which is also when [`Domain::Cxx`] starts carrying two
            // languages, and why the domain is named for the family.
            Lang::Cpp => &["cpp", "cc", "cxx", "h", "hpp", "hh", "hxx"],
            Lang::CSharp => &["cs"],
            Lang::Kotlin => &["kt", "kts"],
            Lang::Swift => &["swift"],
            Lang::Ruby => &["rb"],
            Lang::Php => &["php"],
            Lang::Rust => &["rs"],
            Lang::Scala => &["scala", "sc"],
            Lang::Dart => &["dart"],
            Lang::Elixir => &["ex", "exs"],
            Lang::Haskell => &["hs"],
            Lang::Lua => &["lua"],
            // Not `bats`: the shell grammar does not reject a `.bats`
            // file, it misreads one. Measured against the pinned grammar, a
            // `@test "name" { ... }` block parses with zero error nodes, but
            // the `@test` header and the closing brace each come out as an
            // ordinary `command` — where a real shell function comes out as
            // one `function_definition`. Claiming the extension would hand
            // the walk a file that parses into records for things it does
            // not declare, which is worse than not reading it.
            Lang::Bash => &["sh", "bash"],
            Lang::Hcl => &["tf"],
        }
    }

    /// Whether this language owns files with this extension (no dot).
    pub fn owns_extension(self, ext: &str) -> bool {
        self.extensions().contains(&ext)
    }

    /// The single language owning this extension (no dot), if any.
    pub fn for_extension(ext: &str) -> Option<Lang> {
        Lang::ALL.iter().copied().find(|l| l.owns_extension(ext))
    }
}

/// A canonical fully-qualified name in one domain.
///
/// The grammar is the language resolver's; the core only moves the string
/// and hashes it. What the core does impose is that the grammar be
/// injective within its domain (or the duplicate be declared mergeable),
/// keep container and definition namespaces apart, and be composed only of
/// facts that an unrelated edit cannot move.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fqn(String);

impl Fqn {
    /// Wrap a canonical name a resolver built.
    pub fn new(s: impl Into<String>) -> Self {
        Fqn(s.into())
    }

    /// The name, for hashing and printing.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Take the name back out, for storage.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Which declaration table a reference consults.
///
/// Independent of [`RefKind`]: `kind` is what the site *does*, `space` is
/// which table it reads. Go declares everything in one space and sets
/// [`DeclSpace::Value`] throughout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeclSpace {
    /// Values: functions, methods, variables, constants.
    Value = 0,
    /// Types.
    Type = 1,
    /// Namespaces, packages and modules.
    Namespace = 2,
}

impl DeclSpace {
    /// Stable one-byte storage code. Never renumber.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Inverse of [`DeclSpace::code`]. `None` for codes no variant carries.
    pub fn from_code(c: u8) -> Option<DeclSpace> {
        Some(match c {
            0 => DeclSpace::Value,
            1 => DeclSpace::Type,
            2 => DeclSpace::Namespace,
            _ => return None,
        })
    }
}

/// What a reference can *do* with a definition. Everything else is a facet.
///
/// Deliberately small: a variant earns its place only when some reference
/// site behaves differently at the name. Class-versus-interface, static,
/// abstract and the rest are [`DefFacets`], read by the resolver that cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    /// A free function.
    Function = 0,
    /// A method with a receiver or an owning type.
    Method = 1,
    /// A named type.
    Type = 2,
    /// A constant.
    Const = 3,
    /// A variable.
    Var = 4,
    /// A constructor.
    Constructor = 5,
    /// A field of a type.
    Field = 6,
    /// A property — an accessor pair that reads as a field.
    Property = 7,
    /// A module or package: the container a file's definitions live in.
    Module = 8,
    /// An alias for another definition.
    Alias = 9,
}

impl DefKind {
    /// Stable one-byte storage code. Never renumber.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Human-readable name, for report and query output.
    pub fn name(self) -> &'static str {
        match self {
            DefKind::Function => "function",
            DefKind::Method => "method",
            DefKind::Type => "type",
            DefKind::Const => "const",
            DefKind::Var => "var",
            DefKind::Constructor => "constructor",
            DefKind::Field => "field",
            DefKind::Property => "property",
            DefKind::Module => "module",
            DefKind::Alias => "alias",
        }
    }

    /// Inverse of [`DefKind::code`]. `None` for codes no variant carries.
    pub fn from_code(c: u8) -> Option<DefKind> {
        Some(match c {
            0 => DefKind::Function,
            1 => DefKind::Method,
            2 => DefKind::Type,
            3 => DefKind::Const,
            4 => DefKind::Var,
            5 => DefKind::Constructor,
            6 => DefKind::Field,
            7 => DefKind::Property,
            8 => DefKind::Module,
            9 => DefKind::Alias,
            _ => return None,
        })
    }
}

/// Declaration attributes that no shared `match` branches on.
///
/// A bitset rather than a dependency: a dozen flags do not justify a crate,
/// and the alternative — one [`DefKind`] variant per attribute — makes every
/// shared `match` language-specific.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DefFacets(u16);

impl DefFacets {
    /// Declared static: not dispatched on an instance.
    pub const STATIC: DefFacets = DefFacets(1 << 0);
    /// Declared without a body.
    pub const ABSTRACT: DefFacets = DefFacets(1 << 1);
    /// The type is an interface.
    pub const INTERFACE: DefFacets = DefFacets(1 << 2);
    /// The type is an enumeration.
    pub const ENUM: DefFacets = DefFacets(1 << 3);
    /// The type is a record.
    pub const RECORD: DefFacets = DefFacets(1 << 4);
    /// The type is an annotation.
    pub const ANNOTATION: DefFacets = DefFacets(1 << 5);
    /// Visible outside its container.
    pub const EXPORTED: DefFacets = DefFacets(1 << 6);
    /// Present at runtime. The negation is what keeps erased constructs out
    /// of the call graph while leaving them nodes in the type space.
    pub const RUNTIME: DefFacets = DefFacets(1 << 7);
    /// The last parameter is variadic.
    pub const VARARGS: DefFacets = DefFacets(1 << 8);
    /// Synthesized by the extractor from a language rule rather than written.
    pub const SYNTHETIC: DefFacets = DefFacets(1 << 9);
    /// A constant enumeration, inlined at every use site.
    pub const CONST_ENUM: DefFacets = DefFacets(1 << 10);
    /// Visible only inside the declaration that wrote it, and so **not
    /// inherited** by anything below it.
    ///
    /// The negation of [`DefFacets::EXPORTED`] cannot say this: "not public"
    /// covers three of JLS §6.6.1's four levels, and only the narrowest of
    /// them takes a member out of a subtype's inherited set. A resolver
    /// reading this bit removes a candidate; a resolver reading its absence
    /// learns nothing, which is the honest asymmetry — a language that does
    /// not set it is a language whose closures are unchanged.
    pub const PRIVATE: DefFacets = DefFacets(1 << 11);

    /// The raw bits, for storage.
    pub fn bits(self) -> u16 {
        self.0
    }

    /// Rebuild from stored bits.
    pub fn from_bits(b: u16) -> Self {
        DefFacets(b)
    }

    /// Whether every flag in `other` is set here.
    pub fn contains(self, other: DefFacets) -> bool {
        self.0 & other.0 == other.0
    }

    /// Both sets of flags.
    pub fn union(self, other: DefFacets) -> DefFacets {
        DefFacets(self.0 | other.0)
    }
}

/// A callable's parameter shape, for languages that discriminate by arity.
///
/// `None` on a [`Definition`] means the language does not — Go, JavaScript,
/// TypeScript and Python all answer that way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    /// How many parameters are declared.
    pub count: u32,
    /// Whether the last one is variadic.
    pub varargs: bool,
    /// Source-level parameter type names, in order.
    pub types: Vec<String>,
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

/// A named declaration extracted from one file. Extractor output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// What kind of declaration this is.
    pub kind: DefKind,
    /// The declared name, unqualified.
    pub name: String,
    /// Owner type/class chain, outermost first. Go methods put the receiver
    /// here; nesting languages put the enclosing types here.
    pub owner: Vec<String>,
    /// Which declaration table this lands in.
    pub space: DeclSpace,
    /// Attributes no shared code branches on.
    pub facets: DefFacets,
    /// Parameter shape, when the language discriminates by arity.
    pub params: Option<Params>,
    /// Where the declaration sits.
    pub span: Span,
}

/// What kind of naming a reference site performs.
///
/// `kind` is what the site *does*; [`RefTarget`] is what it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// A call site.
    Call = 0,
    /// An import declaration.
    Import = 1,
    /// A type used in a signature, a declaration, or a conversion.
    TypeUse = 2,
    /// A supertype named by an `extends`/`implements`/base-class clause.
    Inherit = 3,
    /// An object creation site.
    New = 4,
    /// A re-export.
    Export = 5,
    /// A field read or write.
    FieldAccess = 6,
    /// An annotation or decorator applied to a declaration.
    Annotation = 7,
    /// A method referenced rather than invoked.
    MethodRef = 8,
    /// A site that *changes* what the target names, such as a monkeypatch.
    Rebind = 9,
}

impl RefKind {
    /// Stable one-byte storage code. Never renumber.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Human-readable name, for report and query output.
    pub fn name(self) -> &'static str {
        match self {
            RefKind::Call => "call",
            RefKind::Import => "import",
            RefKind::TypeUse => "type-use",
            RefKind::Inherit => "inherit",
            RefKind::New => "new",
            RefKind::Export => "export",
            RefKind::FieldAccess => "field-access",
            RefKind::Annotation => "annotation",
            RefKind::MethodRef => "method-ref",
            RefKind::Rebind => "rebind",
        }
    }

    /// Inverse of [`RefKind::code`]. `None` for codes no variant carries.
    pub fn from_code(c: u8) -> Option<RefKind> {
        Some(match c {
            0 => RefKind::Call,
            1 => RefKind::Import,
            2 => RefKind::TypeUse,
            3 => RefKind::Inherit,
            4 => RefKind::New,
            5 => RefKind::Export,
            6 => RefKind::FieldAccess,
            7 => RefKind::Annotation,
            8 => RefKind::MethodRef,
            9 => RefKind::Rebind,
            _ => return None,
        })
    }
}

/// What the leftmost thing at a reference site is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetRoot {
    /// The leftmost segment is a name: `segments[0]`.
    Name,
    /// `this.m()`, `self.m()`, `Outer.this.m()`.
    This {
        /// The type path qualifying `this`, when the language permits one.
        qualifier: Vec<String>,
    },
    /// `super.m()`, `Iface.super.m()`, `super().m()`.
    Super {
        /// The type path qualifying `super`, when the language permits one.
        qualifier: Vec<String>,
    },
    /// The root is not a name: `f().m()`, `m[k].M()`, `(a+b).c`.
    Expr,
}

/// The shape of what a reference site names, as far as one file can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefTarget {
    /// What the leftmost thing is.
    pub root: TargetRoot,
    /// Dotted path after the root, in source order. May be empty.
    ///
    /// For [`TargetRoot::Name`] the leftmost name is `segments[0]`, so
    /// `Foo()` is one segment and `pkg.Foo()` is two.
    pub segments: Vec<String>,
}

/// The nearest *nameable* enclosing definition of a reference site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encloser {
    /// Path to the definition, outermost first: `["Recv", "Method"]`,
    /// `["Serve"]`.
    pub path: Vec<String>,
    /// What kind of definition it is.
    pub kind: DefKind,
}

impl Encloser {
    /// The synthetic [`Definition`] a resolver's FQN function is applied to
    /// when the caller needs an edge source. `None` when `path` is empty.
    ///
    /// The span is zeroed on purpose: no FQN may be composed of a fact that
    /// an unrelated edit moves, and a span is exactly such a fact. If a
    /// language's FQN function ever reads the span, that language's grammar
    /// is already wrong.
    pub fn as_definition(&self) -> Option<Definition> {
        let (name, owner) = self.path.split_last()?;
        Some(Definition {
            kind: self.kind,
            name: name.clone(),
            owner: owner.to_vec(),
            space: DeclSpace::Value,
            facets: DefFacets::default(),
            params: None,
            span: Span {
                byte_start: 0,
                byte_end: 0,
                line: 0,
            },
        })
    }
}

/// A site in one file that names something possibly defined elsewhere.
///
/// The extractor emits these; only the resolver may turn one into an edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// What kind of naming this is.
    pub kind: RefKind,
    /// Which declaration table it consults.
    pub space: DeclSpace,
    /// The literal text at the site — the store's dedup key component.
    pub raw_target: String,
    /// The parsed shape of the target.
    pub target: RefTarget,
    /// The extractor's file-local verdict: some enclosing block binds
    /// `target.segments[0]`. A *fact*, not a decision — the resolver still
    /// owns the outcome.
    pub locally_bound: bool,
    /// Argument count at a call or creation site. `None` when the language
    /// does not discriminate by arity.
    pub argc: Option<u32>,
    /// The nearest nameable enclosing definition, or `None` when there is
    /// none — package level, a Go `init` body, a Java static initializer.
    pub enclosing: Option<Encloser>,
    /// Where the reference sits.
    pub span: Span,
}

/// Stable one-byte storage code for an [`UnresolvedReason`]. Never renumber.
pub fn reason_code(r: &UnresolvedReason) -> u8 {
    match r {
        UnresolvedReason::DynamicDispatch => 0,
        UnresolvedReason::Generated => 1,
        UnresolvedReason::UnknownPackage => 2,
        UnresolvedReason::TierTwoLanguage => 3,
        UnresolvedReason::NoMatchingDefinition => 4,
        UnresolvedReason::NeedsTypeInference => 5,
        UnresolvedReason::LocalBinding => 6,
        UnresolvedReason::NeedsReceiverType => 7,
        UnresolvedReason::NeedsExpressionType => 8,
        UnresolvedReason::UnindexedSupertype => 9,
        UnresolvedReason::AmbiguousExport => 10,
        UnresolvedReason::WildcardImport => 11,
        UnresolvedReason::DynamicModuleSpecifier => 12,
        UnresolvedReason::ModuleNotFound => 13,
        UnresolvedReason::NotExported => 14,
        UnresolvedReason::AmbiguousOverload => 15,
        UnresolvedReason::AmbiguousName => 16,
        UnresolvedReason::ProjectLayoutUnknown => 17,
        UnresolvedReason::AliasCycle => 18,
    }
}

/// Inverse of [`reason_code`]. `None` for codes no variant carries.
pub fn reason_from_code(c: u8) -> Option<UnresolvedReason> {
    Some(match c {
        0 => UnresolvedReason::DynamicDispatch,
        1 => UnresolvedReason::Generated,
        2 => UnresolvedReason::UnknownPackage,
        3 => UnresolvedReason::TierTwoLanguage,
        4 => UnresolvedReason::NoMatchingDefinition,
        5 => UnresolvedReason::NeedsTypeInference,
        6 => UnresolvedReason::LocalBinding,
        7 => UnresolvedReason::NeedsReceiverType,
        8 => UnresolvedReason::NeedsExpressionType,
        9 => UnresolvedReason::UnindexedSupertype,
        10 => UnresolvedReason::AmbiguousExport,
        11 => UnresolvedReason::WildcardImport,
        12 => UnresolvedReason::DynamicModuleSpecifier,
        13 => UnresolvedReason::ModuleNotFound,
        14 => UnresolvedReason::NotExported,
        15 => UnresolvedReason::AmbiguousOverload,
        16 => UnresolvedReason::AmbiguousName,
        17 => UnresolvedReason::ProjectLayoutUnknown,
        18 => UnresolvedReason::AliasCycle,
        _ => return None,
    })
}

/// Human-readable name for a reason code, for report output.
pub fn reason_name(c: u8) -> &'static str {
    match c {
        0 => "DynamicDispatch",
        1 => "Generated",
        2 => "UnknownPackage",
        3 => "TierTwoLanguage",
        4 => "NoMatchingDefinition",
        5 => "NeedsTypeInference",
        6 => "LocalBinding",
        7 => "NeedsReceiverType",
        8 => "NeedsExpressionType",
        9 => "UnindexedSupertype",
        10 => "AmbiguousExport",
        11 => "WildcardImport",
        12 => "DynamicModuleSpecifier",
        13 => "ModuleNotFound",
        14 => "NotExported",
        15 => "AmbiguousOverload",
        16 => "AmbiguousName",
        17 => "ProjectLayoutUnknown",
        18 => "AliasCycle",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_is_deterministic() {
        assert_eq!(
            node_id(Domain::Go, "m/pkg#Foo"),
            node_id(Domain::Go, "m/pkg#Foo")
        );
    }

    #[test]
    fn node_id_separates_fqns_and_domains() {
        assert_ne!(
            node_id(Domain::Go, "m/pkg#Foo"),
            node_id(Domain::Go, "m/pkg#Bar")
        );
        // The domain byte is a hash input, so one FQN in two identity spaces
        // is two nodes.
        assert_ne!(node_id(Domain::Go, "a"), node_id(Domain::Jvm, "a"));
    }

    #[test]
    fn node_id_hashes_the_domain() {
        // Captured from the tree before `node_id` took a `Domain`, where the
        // hash input was `Lang::Go.code()`. `Domain::Go.code()` is the same
        // byte, so every stored id survives the change — and reordering the
        // hash inputs later would silently re-key the whole store, which is
        // what this literal is here to catch.
        //
        // The input string is the one the bytes were captured from and is
        // deliberately left as it was written. It is an opaque argument
        // here, not an FQN this build would construct: what is pinned is
        // the hash of *these* bytes, so rewriting it to match a later FQN
        // grammar would retire the only assertion that can catch a re-key.
        assert_eq!(
            node_id(Domain::Go, "m/pkg.Foo"),
            [
                63, 214, 32, 37, 252, 63, 136, 181, 240, 215, 200, 74, 2, 158, 3, 33
            ]
        );
    }

    #[test]
    fn a_language_hashes_in_its_own_domain() {
        assert_eq!(Lang::Go.domain(), Domain::Go);
        assert_eq!(Lang::Go.domain().code(), Lang::Go.code());
        assert_eq!(Lang::Java.domain(), Domain::Jvm);
        assert_eq!(Lang::Python.domain(), Domain::Python);
    }

    #[test]
    fn every_tier_two_language_has_its_own_identity_space() {
        // One domain per registered language family. `Cxx` is the only one
        // written to take a second language — C, later, on the same track —
        // and no tier-2 language shares a space with a tier-1 one: Kotlin and
        // Scala are not in `Jvm`, because nothing resolves across the two and
        // a shared space would commit Java's FQN grammar to their names.
        let pairs = [
            (Lang::Cpp, Domain::Cxx),
            (Lang::CSharp, Domain::CSharp),
            (Lang::Kotlin, Domain::Kotlin),
            (Lang::Swift, Domain::Swift),
            (Lang::Ruby, Domain::Ruby),
            (Lang::Php, Domain::Php),
            (Lang::Rust, Domain::Rust),
            (Lang::Scala, Domain::Scala),
            (Lang::Dart, Domain::Dart),
            (Lang::Elixir, Domain::Elixir),
            (Lang::Haskell, Domain::Haskell),
            (Lang::Lua, Domain::Lua),
            (Lang::Bash, Domain::Shell),
            (Lang::Hcl, Domain::Hcl),
        ];
        for (lang, domain) in pairs {
            assert_eq!(lang.domain(), domain, "{} moved domain", lang.name());
        }
        // Domain codes are hash inputs, so they are append-only too: the four
        // tier-1 spaces keep 0–3 and the tier-2 ones start above them.
        assert_eq!(Domain::Go.code(), 0);
        assert_eq!(Domain::Jvm.code(), 1);
        assert_eq!(Domain::EcmaScript.code(), 2);
        assert_eq!(Domain::Python.code(), 3);
        // Exactly `4..=17`, in that order: contiguous, no tier-1 code
        // reused, and — the sequence being strictly increasing — every code
        // distinct, which is what says no two languages share a domain.
        let codes: Vec<u8> = pairs.iter().map(|(_, d)| d.code()).collect();
        assert_eq!(codes, (4u8..=17).collect::<Vec<u8>>());
    }

    #[test]
    fn the_tier_one_set_is_the_ratified_five_and_every_other_language_is_tier_two() {
        // The tier decides what a rate is a rate *of*, so this list is a
        // capability claim and not a convenience: adding a name here says
        // that language's track resolves a call graph, and a report reader
        // will read its number against Go's.
        let tier_one = [
            Lang::Go,
            Lang::Java,
            Lang::JavaScript,
            Lang::TypeScript,
            Lang::Python,
        ];
        for lang in Lang::ALL {
            let want = if tier_one.contains(lang) { 1 } else { 2 };
            assert_eq!(lang.tier(), want, "{} is not tier {want}", lang.name());
        }
        // The phrase a report prints is derived from the number, so a
        // language cannot be labelled one tier and described as the other.
        assert_eq!(Lang::Go.rate_scope(), "call-graph resolution");
        assert_eq!(Lang::Ruby.rate_scope(), "import resolution");
    }

    #[test]
    fn language_codes_are_the_committed_order_and_round_trip() {
        // Storage bytes. Appending is the only permitted change; a
        // renumbering re-keys every stored row silently.
        assert_eq!(Lang::Go.code(), 0);
        assert_eq!(Lang::Java.code(), 1);
        assert_eq!(Lang::JavaScript.code(), 2);
        assert_eq!(Lang::TypeScript.code(), 3);
        assert_eq!(Lang::Python.code(), 4);
        // The ratified tier-2 order, appended above them and equally frozen.
        assert_eq!(Lang::Cpp.code(), 5);
        assert_eq!(Lang::CSharp.code(), 6);
        assert_eq!(Lang::Kotlin.code(), 7);
        assert_eq!(Lang::Swift.code(), 8);
        assert_eq!(Lang::Ruby.code(), 9);
        assert_eq!(Lang::Php.code(), 10);
        assert_eq!(Lang::Rust.code(), 11);
        assert_eq!(Lang::Scala.code(), 12);
        assert_eq!(Lang::Dart.code(), 13);
        assert_eq!(Lang::Elixir.code(), 14);
        assert_eq!(Lang::Haskell.code(), 15);
        assert_eq!(Lang::Lua.code(), 16);
        assert_eq!(Lang::Bash.code(), 17);
        assert_eq!(Lang::Hcl.code(), 18);
        for lang in Lang::ALL {
            assert_eq!(Lang::from_code(lang.code()), Some(*lang));
        }
        // `ALL` is in code order and has no gaps, so the registry walking it
        // walks every storage byte exactly once.
        let codes: Vec<u8> = Lang::ALL.iter().map(|l| l.code()).collect();
        assert_eq!(codes, (0u8..=18).collect::<Vec<u8>>());
        assert_eq!(Lang::ALL.len(), 19);
        assert_eq!(Lang::from_code(19), None);
    }

    #[test]
    fn javascript_and_typescript_share_a_domain_and_nothing_else() {
        // One identity space, because a `.ts` file naming a definition in a
        // `.js` file has to probe an identity that can exist.
        assert_eq!(Lang::JavaScript.domain(), Domain::EcmaScript);
        assert_eq!(Lang::TypeScript.domain(), Domain::EcmaScript);
        // Two languages, because a rate is per language and never
        // aggregated: one EcmaScript number would let a collapse in one of
        // them be masked by the other.
        assert_ne!(Lang::JavaScript, Lang::TypeScript);
        assert_ne!(Lang::JavaScript.code(), Lang::TypeScript.code());
        assert_ne!(Lang::JavaScript.name(), Lang::TypeScript.name());
        // A language code is not a domain code, and only Go's coincide.
        assert_ne!(Lang::TypeScript.code(), Lang::TypeScript.domain().code());
    }

    #[test]
    fn every_extension_has_exactly_one_owner() {
        let mut seen: Vec<&str> = Vec::new();
        for lang in Lang::ALL {
            for ext in lang.extensions() {
                assert!(!seen.contains(ext), "two languages claim `.{ext}`");
                seen.push(ext);
                assert_eq!(Lang::for_extension(ext), Some(*lang));
            }
        }
        assert_eq!(Lang::for_extension("go"), Some(Lang::Go));
        assert_eq!(Lang::for_extension("java"), Some(Lang::Java));
        for ext in ["js", "mjs", "cjs"] {
            assert_eq!(Lang::for_extension(ext), Some(Lang::JavaScript));
        }
        assert_eq!(Lang::for_extension("ts"), Some(Lang::TypeScript));
        assert_eq!(Lang::for_extension("py"), Some(Lang::Python));
        assert_eq!(Lang::for_extension("rs"), Some(Lang::Rust));
        // `.h` resolves to `Cpp`: the amendment the tier-2 registration
        // reserved, made by the commit that first parses it — see
        // [`Lang::extensions`] for the measurement that forced it.
        assert_eq!(Lang::for_extension("h"), Some(Lang::Cpp));
        // Unclaimed, and each on purpose. `.c` is C, not C++: read on the
        // `Cpp` track it would be read under the wrong grammar, so it
        // waits for C's own decision. `.bats` is not shell:
        // the shell grammar parses it without complaint and misreads it,
        // which yields wrong records rather than none. `None` says so in
        // every case rather than handing the file to a near-miss claimant.
        assert_eq!(Lang::for_extension("c"), None);
        assert_eq!(Lang::for_extension("bats"), None);
        assert_eq!(Lang::for_extension(""), None);
        // A `.d.ts` file's extension *is* `ts`; the `.d` is part of the stem.
        assert_eq!(
            std::path::Path::new("a/b/types.d.ts")
                .extension()
                .and_then(|e| e.to_str())
                .and_then(Lang::for_extension),
            Some(Lang::TypeScript),
        );
    }

    #[test]
    fn reason_codes_round_trip() {
        for c in 0u8..=18 {
            let r = reason_from_code(c).expect("code maps to a variant");
            assert_eq!(reason_code(&r), c);
            assert_ne!(reason_name(c), "Unknown");
        }
        assert_eq!(reason_from_code(19), None);
        assert_eq!(reason_name(19), "Unknown");
    }

    #[test]
    fn generated_keeps_macro_generateds_wire_code() {
        // A rename is not a renumbering: every stored `1` still decodes.
        assert_eq!(reason_code(&UnresolvedReason::Generated), 1);
        assert_eq!(reason_from_code(1), Some(UnresolvedReason::Generated));
    }

    #[test]
    fn ref_kind_and_def_kind_codes_are_stable_and_round_trip() {
        // The five original `DefKind` codes and the two original `RefKind`
        // codes are stored bytes; appending must not move them.
        assert_eq!(DefKind::Function.code(), 0);
        assert_eq!(DefKind::Method.code(), 1);
        assert_eq!(DefKind::Type.code(), 2);
        assert_eq!(DefKind::Const.code(), 3);
        assert_eq!(DefKind::Var.code(), 4);
        assert_eq!(RefKind::Call.code(), 0);
        assert_eq!(RefKind::Import.code(), 1);
        for c in 0u8..=9 {
            assert_eq!(DefKind::from_code(c).expect("def kind").code(), c);
            assert_eq!(RefKind::from_code(c).expect("ref kind").code(), c);
        }
        assert_eq!(DefKind::from_code(10), None);
        assert_eq!(RefKind::from_code(10), None);
        for c in 0u8..=2 {
            assert_eq!(DeclSpace::from_code(c).expect("space").code(), c);
        }
        assert_eq!(DeclSpace::from_code(3), None);
    }

    #[test]
    fn facets_are_independent_flags() {
        let f = DefFacets::STATIC.union(DefFacets::EXPORTED);
        assert!(f.contains(DefFacets::STATIC));
        assert!(f.contains(DefFacets::EXPORTED));
        assert!(!f.contains(DefFacets::ABSTRACT));
        assert_eq!(DefFacets::from_bits(f.bits()), f);
        assert!(!DefFacets::default().contains(DefFacets::STATIC));
    }

    #[test]
    fn encloser_as_definition_builds_owner_and_name() {
        let method = Encloser {
            path: vec!["Recv".into(), "Handle".into()],
            kind: DefKind::Method,
        };
        let def = method.as_definition().expect("a nameable encloser");
        assert_eq!(def.owner, ["Recv"]);
        assert_eq!(def.name, "Handle");
        assert_eq!(def.kind, DefKind::Method);
        // Zeroed on purpose: an FQN may not be composed of a fact an
        // unrelated edit moves.
        assert_eq!(def.span.byte_start, 0);
        assert_eq!(def.span.line, 0);

        let function = Encloser {
            path: vec!["Serve".into()],
            kind: DefKind::Function,
        };
        let def = function.as_definition().expect("a nameable encloser");
        assert!(def.owner.is_empty());
        assert_eq!(def.name, "Serve");

        let nothing = Encloser {
            path: vec![],
            kind: DefKind::Function,
        };
        assert_eq!(nothing.as_definition(), None);
    }
}
