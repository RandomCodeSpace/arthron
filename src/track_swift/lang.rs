//! Swift's [`Language`] impl: the constants the track is reported under, the
//! three types only Swift's own layers may read, and the FQN grammar every one
//! of them agrees on.
//!
//! # The FQN grammar
//!
//! Swift's namespace is the **module**, and a module is a SwiftPM target. So
//! every identity begins with one:
//!
//! - A **module**, written as the target's name — `Alamofire`,
//!   `AlamofireTests`. This is what an `import` names and the only kind of
//!   node an import can resolve to.
//! - A file no target claims is its own module, written
//!   [`ORPHAN`](crate::track_swift::project::ORPHAN) + its repo-relative path
//!   without `.swift`: `$Package`, `$Package@swift-6.0`. SwiftPM compiles
//!   each manifest as a module of its own, and `$` is the one reserved
//!   character — a Swift identifier may not begin with it, so a target's
//!   identity can never collide with one of these.
//! - Everything else is its module, its owner chain and its own name, joined
//!   with `.`: `Alamofire.Session`, `Alamofire.Session.request(_:method:)`,
//!   `Alamofire.URLRequest.method` for a member an extension declares on a
//!   type Foundation owns.
//!
//! A module name carries no `.` — SwiftPM target names are identifiers — so a
//! module FQN and a member FQN can never be spelled the same way. That is the
//! whole of the guarantee: an **owner** segment is whatever the declaration's
//! head spells, and an extension's head need not be an identifier at all.
//! `extension [HTTPHeader]` and `extension Collection<String>` really do give
//! `Alamofire.[HTTPHeader].index(of:)` and
//! `Alamofire.Collection<String>.qualityEncoded()`, and `extension Collection`
//! is a third identity rather than the same one. See
//! [`crate::track_swift`]'s known limits, and `tests/swift_corpus.rs`, which
//! pins two of the four the measured corpus contains.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_swift::extract::SwiftHeader;
use crate::track_swift::project::SwiftPackage;
use crate::track_swift::resolve::SwiftScope;

/// The Swift language. Stateless; only its associated types carry anything.
pub struct SwiftLang;

impl Language for SwiftLang {
    const LANG: Lang = Lang::Swift;
    const DOMAIN: Domain = Domain::Swift;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Swift owns and this one cannot drift apart.
    ///
    /// `.swift` and nothing else. A SwiftPM manifest is a `.swift` file and is
    /// read as one — it is an ordinary Swift program that the package manager
    /// runs, not a configuration dialect — so it needs no claim of its own.
    fn extensions() -> &'static [&'static str] {
        Lang::Swift.extensions()
    }

    /// Directories holding build output or vendored dependencies. Descending
    /// into one would index a dependency as if the repository had written it,
    /// inventing in-repository definitions that inflate the resolution rate.
    fn skip_dirs() -> &'static [&'static str] {
        &[".build", "Pods", "Carthage"]
    }

    type Header = SwiftHeader;
    type Scope = SwiftScope;
    type Config = SwiftPackage;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swift_reports_as_swift_and_hashes_in_the_swift_domain() {
        assert_eq!(SwiftLang::LANG, Lang::Swift);
        assert_eq!(SwiftLang::DOMAIN, Domain::Swift);
        assert_eq!(SwiftLang::LANG.domain(), SwiftLang::DOMAIN);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(SwiftLang::extensions(), Lang::Swift.extensions());
        assert_eq!(SwiftLang::extensions(), ["swift"]);
    }

    #[test]
    fn build_output_and_vendored_dependencies_are_never_descended_into() {
        for dir in [".build", "Pods", "Carthage"] {
            assert!(SwiftLang::skip_dirs().contains(&dir));
        }
    }
}
