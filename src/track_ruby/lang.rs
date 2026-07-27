//! Ruby's [`Language`] impl: the constants the track is reported under, the
//! three types only Ruby's own layers may read, and the FQN grammar every one
//! of them agrees on.
//!
//! # The FQN grammar
//!
//! Ruby names two different kinds of thing, and one identity space has to
//! hold both without either being able to spell the other:
//!
//! - A **feature** — what `require` names, the entry Ruby files in
//!   `$LOADED_FEATURES`. Written `$` + the repo-relative path with `.rb`
//!   stripped: `$lib/rack/utils`, `$test/helper`.
//! - A **constant** and its members, written the way rdoc writes them:
//!   `Rack::Utils`, `Rack::Request#params` for an instance method,
//!   `Rack::Request.parse` for a singleton one.
//!
//! `$` is the one reserved character, with one job: a Ruby constant may not
//! begin with it — `$` opens a global variable — so no constant FQN can ever
//! collide with a feature FQN, whatever a repository names its directories.
//! It also keeps [`crate::pipeline`]'s `external:` prefix unreachable from
//! this domain, since a feature FQN is a path and a constant FQN starts with
//! an uppercase letter.

use crate::lang::Language;
use crate::model::{Domain, Lang};
use crate::track_ruby::extract::RubyHeader;
use crate::track_ruby::project::RubyProject;
use crate::track_ruby::resolve::RubyScope;

/// The Ruby language. Stateless; only its associated types carry anything.
pub struct RubyLang;

impl Language for RubyLang {
    const LANG: Lang = Lang::Ruby;
    const DOMAIN: Domain = Domain::Ruby;

    /// Read off [`Lang::extensions`] rather than restated, so the registry's
    /// view of what Ruby owns and this one cannot drift apart.
    ///
    /// `.gemspec`, `.ru` and `Rakefile` are Ruby source and are deliberately
    /// **not** claimed: the extension list was committed with the tier-2
    /// registration, and the honest moment to widen it is a commit that
    /// measures the files it adds.
    fn extensions() -> &'static [&'static str] {
        Lang::Ruby.extensions()
    }

    /// Directories holding installed gems. Descending into one would index a
    /// dependency as if the repository had written it, inventing in-repository
    /// definitions that inflate the resolution rate.
    fn skip_dirs() -> &'static [&'static str] {
        &["vendor", ".bundle"]
    }

    type Header = RubyHeader;
    type Scope = RubyScope;
    type Config = RubyProject;
}

/// The reserved prefix a feature identity carries, and nothing else may.
pub const FEATURE: char = '$';

/// The feature FQN of a repo-relative path: `lib/rack/utils.rb` →
/// `$lib/rack/utils`.
///
/// Total, because every `.rb` file the walk reaches is a feature whether or
/// not it declares a constant, and a `require_relative` naming an empty file
/// still resolves.
pub fn feature_fqn(rel_path: &str) -> String {
    format!(
        "{FEATURE}{}",
        rel_path.strip_suffix(".rb").unwrap_or(rel_path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruby_reports_as_ruby_and_hashes_in_the_ruby_domain() {
        assert_eq!(RubyLang::LANG, Lang::Ruby);
        assert_eq!(RubyLang::DOMAIN, Domain::Ruby);
        assert_eq!(RubyLang::LANG.domain(), RubyLang::DOMAIN);
    }

    #[test]
    fn the_extension_list_is_the_registrys_own() {
        assert_eq!(RubyLang::extensions(), Lang::Ruby.extensions());
        assert_eq!(RubyLang::extensions(), ["rb"]);
        for unclaimed in ["gemspec", "ru", "rake"] {
            assert!(!RubyLang::extensions().contains(&unclaimed));
        }
    }

    #[test]
    fn a_feature_identity_cannot_be_spelled_by_a_constant() {
        assert_eq!(feature_fqn("lib/rack/utils.rb"), "$lib/rack/utils");
        assert_eq!(feature_fqn("test/helper.rb"), "$test/helper");
        // No `.rb` is still a feature: the walk only offers `.rb`, and a
        // name that lost its suffix must not silently become another file's.
        assert_eq!(feature_fqn("Rakefile"), "$Rakefile");
        // A Ruby constant may not begin with `$`, so the two spaces are
        // disjoint by the language's own grammar rather than by convention.
        assert!(feature_fqn("Rack.rb").starts_with(FEATURE));
    }
}
