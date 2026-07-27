//! The Swift track. **Live.** Owns `.swift`, at **tier 2**.
//!
//! [`TRACK`] carries `scan: Some(`[`resolve::scan_swift`]`)`, so
//! [`crate::registry::Track::owns_extension`] answers `true` for `swift` and
//! the driver runs Swift over every `.swift` file the walk reaches. Four
//! layers, and the boundary between them is the project's first
//! non-negotiable:
//!
//! - [`extract`] — one file in, records out, **never an edge**.
//! - [`project`] — phase 0: which modules this package builds, and which files
//!   each one is made of. Swift states neither in its source, so this is what
//!   one `go.mod` line is to Go — except that it also decides, for every file
//!   in the tree, which namespace its declarations land in.
//! - [`lang`] — the [`crate::lang::Language`] impl and the FQN grammar the
//!   other three agree on.
//! - [`resolve`] — the one place a Swift [`crate::Outcome`] is produced. Every
//!   reference ends `Resolved`, `External`, or `Unresolved(reason)`, and there
//!   is no way to express "dropped".
//!
//! # What tier 2 means here, precisely
//!
//! Definitions, structure, and imports. **No call edges and no type-use
//! resolution**, and the honest consequence is that the extractor emits no
//! call, type or inheritance reference *at all*: a tier-2 language that
//! emitted them un-gated would put references into a denominator nothing in
//! this track resolves, and report tier-1 coverage it has not measured.
//!
//! So Swift's gate is an **import-resolution rate**, and it is not comparable
//! with Go's or Java's, or with Ruby's or Rust's.
//!
//! # How to read Swift's rate, which is the unusual part of this track
//!
//! **The denominator is small, and that is the measurement, not a defect.**
//! A Swift module is a whole SwiftPM target rather than a file or a directory,
//! and membership is decided by the manifest. In the measured corpus not one
//! of the 43 files in `Source/` imports Alamofire — all 43 *are* Alamofire,
//! and each sees the other 42's top-level names with no import statement, no
//! path and no qualifier anywhere in the referencing file. That is a
//! cross-file visibility relation with **no reference site to extract**, and
//! arthron emits nothing for it rather than synthesising 43×42 edges out of a
//! manifest line.
//!
//! What is left to resolve is the import surface, and most of it points at the
//! platform: 170 import declarations naming 15 distinct modules, of which 14
//! are SDK or toolchain modules with no file in the repository. Only the
//! imports of the package's own target are in-repository references at all. So
//! the rate is taken over a couple of dozen references and reads high, and it
//! would read high for a resolver that did much less. **The deliverable of
//! this track is the definition census beside the rate** — 194 extensions, 74
//! `#if` blocks read as written, and every declaration each of them contains —
//! and `tests/swift_corpus.rs` pins that census exactly, on both sides of the
//! store, because no rate can see it.
//!
//! Two consequences worth stating before someone infers them:
//!
//! - **`local_binding` is zero, and stays zero.** It is the one bucket the
//!   rate's own definition lets a resolver move references into without
//!   linking anything. Tier 2 emits no expression-level reference, so nothing
//!   here *can* name a local; a non-zero count would mean the contract above
//!   had been widened, and the baseline fails on drift in it.
//! - **`external` is large, and it is the platform.** `Foundation`, `XCTest`,
//!   `Dispatch`, `Security` and the rest are modules outside this package, and
//!   the resolver may only say so because the manifest *enumerates* the
//!   modules the package builds — see [`resolve`] for why that enumeration is
//!   what separates this from Ruby's answer, and for the guard that stops an
//!   unread manifest laundering the whole import surface into a bucket outside
//!   both terms of the rate.
//!
//! # Known limits, recorded rather than left to be rediscovered
//!
//! - **Overloads that differ only in parameter types share a node.** A
//!   callable is named the way Swift names a declaration —
//!   `request(_:method:)`, labels included — so overloads that differ in
//!   labels are separate; ones that differ only in types are not.
//! - **`import struct Module.Decl` is implemented and unexercised.** The
//!   measured corpus has 170 imports and every one of them is a plain
//!   `import Module`. The form is resolved by probing the declaration under
//!   its module, which is a rule the fixture tests state and no corpus has
//!   yet confirmed.
//! - **A target's default directory is `Sources/<name>` or `Tests/<name>`.**
//!   Every target in the measured corpus states an explicit `path:`, so the
//!   default is fixture-proven and corpus-unexercised. SwiftPM's other
//!   predefined source directories are not modelled.
//! - **Conditional compilation is a union, never a selection.** Both arms of a
//!   `#if` are read as written, so a member declared once per platform is
//!   several declarations — which is why [`resolve`]'s `mergeable` answers
//!   `false`.
//! - **A constrained extension's `where` clause is not part of the identity.**
//!   `extension AlamofireExtension where ExtendedType: URLSessionConfiguration`
//!   and `… where ExtendedType == SecPolicy` each declare a static named
//!   `default`, and the two share a node. Measured: one of the corpus's six
//!   FQN collisions is exactly this.
//! - **A file-scoped `private` declaration is not scoped to its file in the
//!   identity.** Swift makes a top-level `private` declaration visible only
//!   inside its own file, so two files may declare `private enum
//!   TestCertificates` and mean two types; here they share a node. Measured:
//!   the corpus's other five collisions are exactly this, and the `PRIVATE`
//!   facet is already recorded on both, so the fix is available to whichever
//!   tier needs it.
//!
//! Both of the last two are *surfaced* rather than silent: they arrive as the
//! `fqn collisions` count the report prints, which is what that counter is
//! for, and `tests/swift_corpus.rs` pins the number at six.
//! - **Macro declarations and expansions are not read.** The corpus has 183
//!   macro invocations and no declaration.
//! - **`operator` and `precedencegroup` declarations are not emitted**, and
//!   the corpus contains none of either.
//!
//! Sharing the store with the other live tracks is safe in both directions: a
//! scan forgets only files carrying an extension the running track owns, and
//! extension ownership is a partition (see
//! [`crate::model::Lang::for_extension`]); the manifest fence is per language,
//! and Swift's digest covers exactly what phase 0 read.
//!
//! A baseline is recorded with `arthron gate --rebase`. Swift's rate is
//! Swift's own and is never averaged into anyone else's.

use crate::model::Lang;
use crate::registry::Track;

pub mod extract;
pub mod lang;
pub mod project;
pub mod resolve;

/// Swift's registration. **Live**: the track owns `.swift`, so
/// [`crate::registry::Track::owns_extension`] answers `true` for it and the
/// driver runs [`resolve::scan_swift`] over every Swift file the walk reaches.
pub const TRACK: Track = Track {
    name: "swift",
    langs: &[Lang::Swift],
    scan: Some(resolve::scan_swift_with),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swift_is_registered_and_live() {
        assert!(TRACK.is_enabled());
        assert_eq!(TRACK.langs, [Lang::Swift]);
        assert!(Lang::Swift.owns_extension("swift"));
        // Extension ownership is a property of the language whether or not
        // anything is built for it; whether a scan reads such a file is a
        // property of the track, and the track now says yes.
        assert!(TRACK.owns_extension("swift"));
        // Swift reports one rate, under its own language code, and shares an
        // identity space with nobody.
        assert_eq!(Lang::Swift.domain(), crate::model::Domain::Swift);
    }

    #[test]
    fn going_live_claimed_no_extension_the_registration_had_not() {
        // The tier-2 registration committed `.swift` and nothing else. A
        // SwiftPM manifest is a `.swift` file and is read as one, so no
        // second claim was needed — and `.c` and `.h`, which a Swift target
        // may also contain, stay deliberately unclaimed: they belong to C's
        // own decision, on the C++ track, and reading one here would parse it
        // as the wrong language.
        assert_eq!(Lang::Swift.extensions(), ["swift"]);
        for unclaimed in ["c", "h", "m", "mm", "swiftinterface"] {
            assert!(!TRACK.owns_extension(unclaimed));
        }
    }
}
