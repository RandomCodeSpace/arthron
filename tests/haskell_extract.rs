//! Haskell extractor fixtures: what one file in isolation yields, and — just
//! as load-bearing — what it refuses to yield.
//!
//! Tier 2, best effort: definitions, structure, and import-like references.
//! No call edge, no type use, no expression-level reference. The negative
//! assertions here are the contract; a `Call` or `TypeUse` reference would put
//! sites into a denominator nothing in this track resolves.

use arthron::model::{DefKind, RefKind};
use arthron::track_haskell::extract::extract;

/// Every definition the fixture yields, as `(kind, owner-joined, name, line)`.
fn census(source: &str, rel: &str) -> Vec<(DefKind, String, String, u32)> {
    let facts = extract(rel, source);
    facts
        .defs
        .iter()
        .map(|d| (d.kind, d.owner.join("."), d.name.clone(), d.span.line))
        .collect()
}

#[test]
fn the_module_header_is_the_files_own_node_and_comes_first() {
    let facts = extract("src/Data/Aeson.hs", "module Data.Aeson where\n");
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].name, "Data.Aeson");
    assert_eq!(facts.header.module_name.as_deref(), Some("Data.Aeson"));
    assert_eq!(facts.header.rel_path, "src/Data/Aeson.hs");
}

#[test]
fn a_file_with_no_header_is_module_main_by_the_languages_own_rule() {
    // Haskell 2010 §5.1: a module lacking a header is `module Main(main) where`.
    // The node is still the file's, so a file that declares nothing is still
    // a module an import can name.
    let facts = extract(
        "examples/src/Simplest.hs",
        "main :: IO ()\nmain = pure ()\n",
    );
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].name, "Main");
    assert_eq!(facts.header.module_name, None);
}

#[test]
fn every_import_form_is_one_reference_to_the_module_name() {
    // `qualified`, `as`, `hiding` and a selector list all name one module.
    // The name a site writes is frequently not the name the target declares,
    // and none of those spellings changes what is being named.
    let source = "\
module M where

import Data.Text (Text)
import qualified Data.ByteString as B
import Data.Map as M hiding (map)
import qualified Data.Aeson.Types
";
    let facts = extract("src/M.hs", source);
    let refs: Vec<(RefKind, &str, u32)> = facts
        .refs
        .iter()
        .map(|r| (r.kind, r.raw_target.as_str(), r.span.line))
        .collect();
    assert_eq!(
        refs,
        [
            (RefKind::Import, "Data.Text", 3),
            (RefKind::Import, "Data.ByteString", 4),
            (RefKind::Import, "Data.Map", 5),
            (RefKind::Import, "Data.Aeson.Types", 6),
        ],
    );
    for r in &facts.refs {
        assert_eq!(r.kind, RefKind::Import);
        assert!(!r.locally_bound);
    }
    assert_eq!(facts.header.imports.len(), facts.refs.len());
}

#[test]
fn a_cpp_else_arm_is_invisible_to_the_pinned_grammar() {
    // Measured, and recorded because it is a shortfall rather than a design.
    // tree-sitter-haskell 0.23.1 gives the `#if` arm's contents back as
    // ordinary declarations — the guard line is a `cpp` node beside them — but
    // it swallows `#else` **and everything under it** into a single `cpp`
    // node. So the first arm of every conditional is read and no other one is.
    //
    // The corpus pays 11 import lines for this, in five files, out of 1,085.
    // The alternative is a resolver that pre-processes CPP, which means
    // choosing a GHC version and a set of `MIN_VERSION_*` macros the source
    // never defines — a graph no single reader of the source could see.
    let source = "\
module M where
#ifdef MIN_VERSION_base
import Data.Kind (Type)
data Yes = Yes
#else
import Data.Kind.Old (Type)
data No = No
#endif
import Data.Text
";
    let facts = extract("src/M.hs", source);
    let names: Vec<&str> = facts.refs.iter().map(|r| r.raw_target.as_str()).collect();
    assert_eq!(names, ["Data.Kind", "Data.Text"]);
    let defs: Vec<&str> = facts.defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(defs, ["M", "Yes", "Yes"]);
}

#[test]
fn a_constructor_and_its_type_of_one_name_are_two_definitions() {
    // Haskell's type and value namespaces are disjoint, so `newtype Key = Key`
    // declares two things. A flat name would merge them and lose one.
    let source = "\
module Data.Aeson.Key where

newtype Key = Key { unKey :: Text }
";
    assert_eq!(
        census(source, "src/Data/Aeson/Key.hs"),
        [
            (DefKind::Module, String::new(), "Data.Aeson.Key".into(), 1),
            (DefKind::Type, String::new(), "Key".into(), 3),
            (DefKind::Constructor, "Key".into(), "Key".into(), 3),
            (DefKind::Field, "Key".into(), "unKey".into(), 3),
        ],
    );
}

#[test]
fn a_class_declares_its_methods_and_an_instance_declares_nothing() {
    // An instance body binds names the class already declared; it introduces
    // no name a reference elsewhere could spell, so it mints no node.
    let source = "\
module M where

class ToJSON a where
    toJSON :: a -> Value
    toJSON = genericToJSON
    toEncoding :: a -> Encoding

instance ToJSON Bool where
    toJSON = Bool
";
    assert_eq!(
        census(source, "src/M.hs"),
        [
            (DefKind::Module, String::new(), "M".into(), 1),
            (DefKind::Type, String::new(), "ToJSON".into(), 3),
            // The signature and the default implementation are one method
            // written twice; the resolver merges them, and the extractor is
            // forbidden to make that call.
            (DefKind::Method, "ToJSON".into(), "toJSON".into(), 4),
            (DefKind::Method, "ToJSON".into(), "toJSON".into(), 5),
            (DefKind::Method, "ToJSON".into(), "toEncoding".into(), 6),
        ],
    );
}

#[test]
fn a_local_binding_is_not_a_node() {
    // `where` and `let` bind names no other file can name. Locals are not
    // nodes by decision, and a declaration frame this extractor cannot walk
    // to the top of contributes nothing rather than a guessed owner.
    let source = "\
module M where

go :: Int -> Int
go n = helper n
  where
    helper k = k + offset
    offset = 1
";
    assert_eq!(
        census(source, "src/M.hs"),
        [
            (DefKind::Module, String::new(), "M".into(), 1),
            // `go`'s signature and its equation. Neither `helper` nor
            // `offset` appears at all.
            (DefKind::Function, String::new(), "go".into(), 3),
            (DefKind::Function, String::new(), "go".into(), 4),
        ],
    );
}

#[test]
fn a_signature_and_its_binding_are_one_definition_written_twice() {
    // Both are the same name; the resolver merges them. What matters here is
    // that neither is lost, and that a multi-name signature declares each.
    let source = "\
module M where

x, y :: Int
x = 1
y = 2
";
    assert_eq!(
        census(source, "src/M.hs"),
        [
            (DefKind::Module, String::new(), "M".into(), 1),
            (DefKind::Function, String::new(), "x".into(), 3),
            (DefKind::Function, String::new(), "y".into(), 3),
            (DefKind::Function, String::new(), "x".into(), 4),
            (DefKind::Function, String::new(), "y".into(), 5),
        ],
    );
}

#[test]
fn an_operator_definition_is_named_without_its_parentheses() {
    // `(.:) = …` declares `.:`, and `import Data.Aeson ((.:))` writes the
    // same name inside parentheses. One spelling, chosen once.
    let source = "\
module M where

(.:) :: Object -> Key -> Parser a
(.:) = explicitParseField

infixl 4 .:
";
    assert_eq!(
        census(source, "src/M.hs"),
        [
            (DefKind::Module, String::new(), "M".into(), 1),
            (DefKind::Function, String::new(), ".:".into(), 3),
            (DefKind::Function, String::new(), ".:".into(), 4),
        ],
    );
}

#[test]
fn a_gadt_declares_every_constructor_a_binding_list_names() {
    let source = "\
module M where

data GADT a where
  MkInt :: Int -> GADT Int
  MkBool, MkOther :: Bool -> GADT Bool
";
    assert_eq!(
        census(source, "src/M.hs"),
        [
            (DefKind::Module, String::new(), "M".into(), 1),
            (DefKind::Type, String::new(), "GADT".into(), 3),
            (DefKind::Constructor, "GADT".into(), "MkInt".into(), 4),
            (DefKind::Constructor, "GADT".into(), "MkBool".into(), 5),
            (DefKind::Constructor, "GADT".into(), "MkOther".into(), 5),
        ],
    );
}

#[test]
fn a_data_instance_declares_constructors_and_no_new_type() {
    // `data instance Sing Bool = SBool` adds a constructor to the *family*'s
    // namespace. The family name is already declared; re-declaring it here
    // would be one type counted twice.
    let source = "\
module M where

data family Sing a
data instance Sing Bool = SBool | STrue
";
    assert_eq!(
        census(source, "src/M.hs"),
        [
            (DefKind::Module, String::new(), "M".into(), 1),
            (DefKind::Type, String::new(), "Sing".into(), 3),
            (DefKind::Constructor, "Sing".into(), "SBool".into(), 4),
            (DefKind::Constructor, "Sing".into(), "STrue".into(), 4),
        ],
    );
}

#[test]
fn a_pattern_synonym_is_a_constructor_the_module_declares() {
    let source = "\
module M where

pattern Head :: a -> [a]
pattern Head x <- (x:_)
";
    assert_eq!(
        census(source, "src/M.hs"),
        [
            (DefKind::Module, String::new(), "M".into(), 1),
            (DefKind::Constructor, String::new(), "Head".into(), 3),
            (DefKind::Constructor, String::new(), "Head".into(), 4),
        ],
    );
}

#[test]
fn a_foreign_import_declares_the_haskell_name_it_binds() {
    let source = "\
module M where

foreign import ccall unsafe \"hs_memcpy\" memcpy :: Int -> IO ()
";
    assert_eq!(
        census(source, "src/M.hs"),
        [
            (DefKind::Module, String::new(), "M".into(), 1),
            (DefKind::Function, String::new(), "memcpy".into(), 3),
        ],
    );
}

#[test]
fn a_broken_file_still_yields_its_module_node() {
    // tree-sitter is error-tolerant, and a file that does not parse is still
    // a file an import can name.
    let facts = extract("src/Broken.hs", "module Broken where\ndata ((( \n");
    assert_eq!(facts.defs[0].kind, DefKind::Module);
    assert_eq!(facts.defs[0].name, "Broken");
}

#[test]
fn records_come_out_in_source_order() {
    let source = "\
module M where

import A
data T = T
import B
";
    let facts = extract("src/M.hs", source);
    assert!(
        facts
            .defs
            .windows(2)
            .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
        "{:?}",
        facts.defs,
    );
    assert!(
        facts
            .refs
            .windows(2)
            .all(|w| w[0].span.byte_start <= w[1].span.byte_start),
        "{:?}",
        facts.refs,
    );
}
