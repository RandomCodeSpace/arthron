//! ECMAScript binding environments: is this name bound by some enclosing
//! scope, or does it name something the graph can hold?
//!
//! A *file-local verdict*, and the whole of it. The extractor states the
//! fact; the resolver still owns the outcome.

use crate::model::DeclSpace;
use crate::sg::SgNode;

/// Node kinds that create a function environment.
///
/// A function environment binds its parameters, its `var` and function
/// declarations wherever in the body they are written, and — for a *named
/// function expression* — its own name.
pub fn is_function_like(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "function_expression"
            | "function"
            | "generator_function"
            | "generator_function_declaration"
            | "arrow_function"
            | "method_definition"
            | "method_signature"
            | "abstract_method_signature"
            | "function_signature"
            | "class_static_block"
    )
}

/// Whether a `statement_block` is a *container* body rather than a scope.
///
/// A namespace body and an ambient module body hold declarations that are
/// nodes, so a name declared there is nameable from outside and must not be
/// reported as a local.
fn is_container_body(parent: Option<&SgNode>) -> bool {
    parent.is_some_and(|p| {
        matches!(
            &*p.kind(),
            "internal_module" | "module" | "ambient_declaration" | "global"
        )
    })
}

/// Visit every name a binding pattern binds, short-circuiting when `visit`
/// answers `true`.
///
/// One walk serves both questions asked of a pattern — *does it bind this
/// name* and *what names does it bind* — so the two can never disagree.
///
/// The right-hand side of a default and the key of an object pattern are
/// deliberately not walked: `function f(a = dflt())` binds `a` and names
/// `dflt`, and `{ b: c }` binds `c` and names nothing called `b`. Reading
/// either as a binding would move a real reference into the local bucket,
/// which raises the rate by deleting it from both of the rate's terms.
pub fn pattern_walk(node: &SgNode, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match &*node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => visit(&node.text()),
        "property_identifier"
        | "computed_property_name"
        | "type_annotation"
        | "accessibility_modifier"
        | "comment"
        | "string" => false,
        "assignment_pattern" => node
            .field("left")
            .is_some_and(|left| pattern_walk(&left, visit)),
        "pair_pattern" => match node.field("value") {
            Some(value) => pattern_walk(&value, visit),
            None => node
                .children()
                .filter(|c| c.kind() != "property_identifier")
                .any(|c| pattern_walk(&c, visit)),
        },
        "required_parameter" | "optional_parameter" => match node.field("pattern") {
            Some(pattern) => pattern_walk(&pattern, visit),
            None => node.children().any(|c| pattern_walk(&c, visit)),
        },
        _ => node.children().any(|c| pattern_walk(&c, visit)),
    }
}

/// Whether a binding pattern binds `name`.
fn pattern_binds(node: &SgNode, name: &str) -> bool {
    pattern_walk(node, &mut |bound| bound == name)
}

/// Every name a binding pattern binds, in source order.
pub fn pattern_names(node: &SgNode) -> Vec<String> {
    let mut names = Vec::new();
    pattern_walk(node, &mut |bound| {
        names.push(bound.to_string());
        false
    });
    names
}

/// Whether any `variable_declarator` under a declaration binds `name`.
fn declarators_bind(decl: &SgNode, name: &str) -> bool {
    decl.children()
        .filter(|c| c.kind() == "variable_declarator")
        .any(|d| match d.field("name") {
            Some(pattern) => pattern_binds(&pattern, name),
            None => false,
        })
}

/// Whether a node's `name` field is exactly `name`.
fn named(node: &SgNode, name: &str) -> bool {
    node.field("name").is_some_and(|n| n.text() == name)
}

/// Whether a function-like node's parameter list binds `name`.
///
/// Both spellings: `formal_parameters` for every function form, and the
/// single bare parameter an arrow function may carry instead.
fn params_bind(func: &SgNode, name: &str) -> bool {
    func.children()
        .filter(|c| c.kind() == "formal_parameters")
        .any(|list| list.children().any(|p| pattern_binds(&p, name)))
        || func
            .field("parameter")
            .is_some_and(|p| pattern_binds(&p, name))
}

/// Whether a `var` or function declaration anywhere in a function body binds
/// `name`.
///
/// `VarScopedDeclarations` are instantiated before the body runs, so this is
/// deliberately position-free: `f(); function f(){}` and `v(); var v = 1`
/// both name the local. Nested functions, classes and namespaces are not
/// descended into — their declarations belong to their own environments.
fn var_hoist_binds(node: &SgNode, name: &str) -> bool {
    for child in node.children() {
        let hit = match &*child.kind() {
            "variable_declaration" => declarators_bind(&child, name),
            "function_declaration" | "generator_function_declaration" => named(&child, name),
            kind if is_function_like(kind) => false,
            "class_declaration"
            | "abstract_class_declaration"
            | "class"
            | "class_body"
            | "internal_module" => false,
            _ => var_hoist_binds(&child, name),
        };
        if hit {
            return true;
        }
    }
    false
}

/// Whether a function environment binds `name`.
fn function_scope_binds(func: &SgNode, name: &str, space: DeclSpace) -> bool {
    if space != DeclSpace::Value {
        return false; // type parameters are handled separately
    }
    let kind = func.kind();
    // A named function expression binds its own name inside its body only.
    if matches!(
        &*kind,
        "function_expression" | "function" | "generator_function"
    ) && named(func, name)
    {
        return true;
    }
    // `arguments` is an implicit binding of every non-arrow function.
    if name == "arguments" && kind != "arrow_function" {
        return true;
    }
    params_bind(func, name)
        || func
            .field("body")
            .is_some_and(|body| var_hoist_binds(&body, name))
}

/// Whether the declarations directly inside a block bind `name`.
///
/// `LexicallyScopedDeclarations` cover the whole block regardless of where in
/// it they are written, so this is position-free too.
fn block_binds(block: &SgNode, name: &str, space: DeclSpace) -> bool {
    block.children().any(|c| match &*c.kind() {
        "lexical_declaration" | "variable_declaration" => {
            space == DeclSpace::Value && declarators_bind(&c, name)
        }
        "function_declaration" | "generator_function_declaration" => {
            space == DeclSpace::Value && named(&c, name)
        }
        "class_declaration" | "abstract_class_declaration" | "enum_declaration" => named(&c, name),
        "interface_declaration" | "type_alias_declaration" => {
            space == DeclSpace::Type && named(&c, name)
        }
        _ => false,
    })
}

/// Whether a `switch` body's case clauses bind `name`.
///
/// The whole body is one block: a `let` in one case is in scope in the next.
fn switch_binds(body: &SgNode, name: &str, space: DeclSpace) -> bool {
    body.children()
        .filter(|c| matches!(&*c.kind(), "switch_case" | "switch_default"))
        .any(|clause| block_binds(&clause, name, space))
}

/// Whether a `for (…;…;…)` head binds `name`.
fn for_head_binds(stmt: &SgNode, name: &str, space: DeclSpace) -> bool {
    space == DeclSpace::Value
        && stmt.children().any(|c| {
            matches!(&*c.kind(), "lexical_declaration" | "variable_declaration")
                && declarators_bind(&c, name)
        })
}

/// Whether a `for … in`/`for … of` head binds `name`.
///
/// Only when it *declares*: `for (k of xs)` assigns to a name bound
/// elsewhere and introduces nothing.
fn for_in_head_binds(stmt: &SgNode, name: &str, space: DeclSpace) -> bool {
    space == DeclSpace::Value
        && stmt
            .children()
            .any(|c| matches!(&*c.kind(), "let" | "const" | "var"))
        && stmt
            .field("left")
            .is_some_and(|left| pattern_binds(&left, name))
}

/// Whether a declaration's type parameter list binds `name`.
fn type_parameters_bind(node: &SgNode, name: &str) -> bool {
    node.children()
        .filter(|c| c.kind() == "type_parameters")
        .any(|list| {
            list.children()
                .filter(|p| p.kind() == "type_parameter")
                .any(|p| match p.field("name") {
                    Some(n) => n.text() == name,
                    None => p.children().any(|c| c.text() == name),
                })
        })
}

/// Whether some enclosing binding environment binds `name` at this site.
///
/// A *file-local verdict*, and the whole of it: every ECMAScript binder for a
/// name is decidable from one file's AST, which is why a `bool` is all that
/// crosses the extractor/resolver boundary.
///
/// Two rules are not optional. **Position never decides** — unlike Go, every
/// binding of a scope is instantiated before the scope's code runs, so a
/// reference above a `const` still names it (the read throws, the name still
/// binds). And **module level is not a binding environment**: a top-level
/// declaration is a node, so reporting it local would delete a real reference
/// from both terms of the resolution rate.
///
/// `space` matters because the tables are separate: a `const T` inside a
/// function says nothing about the type `T`, and a `<T>` type parameter says
/// nothing about the value.
pub fn is_locally_bound(node: &SgNode, name: &str, space: DeclSpace) -> bool {
    if name.is_empty() {
        return false;
    }
    let ancestors: Vec<SgNode> = node.ancestors().collect();
    for (i, ancestor) in ancestors.iter().enumerate() {
        if space == DeclSpace::Type && type_parameters_bind(ancestor, name) {
            return true;
        }
        let hit = match &*ancestor.kind() {
            "program" => return false,
            "statement_block" => {
                !is_container_body(ancestors.get(i + 1)) && block_binds(ancestor, name, space)
            }
            "switch_body" => switch_binds(ancestor, name, space),
            "for_statement" => for_head_binds(ancestor, name, space),
            "for_in_statement" => for_in_head_binds(ancestor, name, space),
            "catch_clause" => ancestor
                .field("parameter")
                .is_some_and(|p| pattern_binds(&p, name)),
            // A named class *expression* binds its own name inside its body;
            // a class *declaration* is a node and binds nothing local.
            "class" | "class_expression" => space == DeclSpace::Value && named(ancestor, name),
            kind if is_function_like(kind) => function_scope_binds(ancestor, name, space),
            _ => false,
        };
        if hit {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sg::{Rules, SourceTree};

    /// Whether the call site written exactly `name(` is locally bound.
    fn bound_js(src: &str, name: &str) -> bool {
        bound(false, src, name, DeclSpace::Value)
    }

    fn bound_ts(src: &str, name: &str) -> bool {
        bound(true, src, name, DeclSpace::Value)
    }

    fn bound(ts: bool, src: &str, name: &str, space: DeclSpace) -> bool {
        let tree = if ts {
            SourceTree::parse_typescript(src)
        } else {
            SourceTree::parse_javascript(src)
        };
        let lang = if ts { "typescript" } else { "javascript" };
        let yaml = format!("id: c\nlanguage: {lang}\nrule:\n  kind: call_expression\n");
        let rules = Rules::compile(&yaml).expect("rules compile");
        let dotted = format!("{name}.");
        for (_, node) in tree.matches(&rules) {
            let Some(callee) = node.field("function") else {
                continue;
            };
            if callee.text() == name || callee.text().starts_with(&dotted) {
                return is_locally_bound(&callee, name, space);
            }
        }
        panic!("no call site `{name}(` in:\n{src}");
    }

    /// Whether the type name `name` is bound where it is written.
    fn bound_type(src: &str, name: &str) -> bool {
        let tree = SourceTree::parse_typescript(src);
        let yaml = "id: t\nlanguage: typescript\nrule:\n  kind: type_identifier\n";
        let rules = Rules::compile(yaml).expect("rules compile");
        for (_, node) in tree.matches(&rules) {
            if node.text() == name
                && node
                    .parent()
                    .is_some_and(|p| p.kind() == "type_annotation" || p.kind() == "union_type")
            {
                return is_locally_bound(&node, name, DeclSpace::Type);
            }
        }
        panic!("no type use of `{name}` in:\n{src}");
    }

    #[test]
    fn module_level_is_not_a_binding_environment() {
        // D1/E1: a module-level binding *is* a node. Calling it local would
        // delete a real reference from both terms of the resolution rate.
        assert!(!bound_js("const f = () => {};\nf();\n", "f"));
        assert!(!bound_js("f();\nfunction f(){}\n", "f"));
        assert!(!bound_js("import { p } from './p.js';\np();\n", "p"));
    }

    #[test]
    fn function_declarations_hoist_over_the_whole_function() {
        // D1: `FunctionDeclarationInstantiation` creates and initialises
        // function declarations before any statement runs, so a call above
        // the declaration still names the local one. Unlike Go, position
        // does not decide.
        assert!(bound_js("function f(){ g(); function g(){} }\n", "g"));
    }

    #[test]
    fn a_lexical_binding_binds_before_its_initialiser_too() {
        // D2: `let`/`const` are created uninitialised, so the call throws at
        // runtime — but the *name* still binds to the local. A position
        // check here would invent an edge to a module-level definition the
        // code cannot reach.
        assert!(bound_js("function f(){ h(); const h = () => {}; }\n", "h"));
        assert!(bound_js("function f(){ v(); var v = 1; }\n", "v"));
    }

    #[test]
    fn an_inner_binding_shadows_an_import() {
        // D3, the false-edge case: the inner `parse` is a local, and linking
        // it to `./p.js` is a wrong edge — strictly worse than an unresolved
        // reference, because a miss is counted and a wrong edge is not.
        let src = "import { parse } from './p.js';\nparse();\nfunction f(){ const parse = 1; parse(); }\n";
        let tree = SourceTree::parse_javascript(src);
        let yaml = "id: c\nlanguage: javascript\nrule:\n  kind: call_expression\n";
        let rules = Rules::compile(yaml).expect("rules compile");
        let verdicts: Vec<bool> = tree
            .matches(&rules)
            .into_iter()
            .map(|(_, n)| {
                let callee = n.field("function").expect("callee");
                is_locally_bound(&callee, "parse", DeclSpace::Value)
            })
            .collect();
        assert_eq!(verdicts, [false, true], "module level, then the shadow");
    }

    #[test]
    fn a_sibling_block_binding_does_not_escape() {
        assert!(!bound_js("function f(){ { const s = 1; } s(); }\n", "s"));
        assert!(bound_js("function f(){ { const s = 1; s(); } }\n", "s"));
    }

    #[test]
    fn a_closure_local_is_bound() {
        // D4: closures and callbacks make this constant in JavaScript. It is
        // the reason `LocalBinding` exists as a reason of its own.
        assert!(bound_js("function f(){ const g = () => {}; g(); }\n", "g"));
        assert!(bound_js("arr.forEach(save => save());\n", "save"));
    }

    #[test]
    fn parameters_bind_including_destructured_ones() {
        // D5. The Go extractor collects no parameter names; this one must.
        assert!(bound_js("function f(config){ config.get(); }\n", "config"));
        assert!(bound_js("function f({ a }){ a(); }\n", "a"));
        assert!(bound_js("function f({ b: c }){ c(); }\n", "c"));
        assert!(
            !bound_js("function f({ b: c }){ b(); }\n", "b"),
            "an object pattern binds the value name, never the key"
        );
        assert!(bound_js("function f([ d ]){ d(); }\n", "d"));
        assert!(bound_js("function f(...rest){ rest(); }\n", "rest"));
        assert!(bound_js("function f(a = 1){ a(); }\n", "a"));
        assert!(
            !bound_js("function f(a = dflt()){ }\n", "dflt"),
            "a default initialiser is an expression, not a binding"
        );
        assert!(bound_js("const g = (x) => x();\n", "x"));
    }

    #[test]
    fn a_catch_parameter_binds_its_clause() {
        // D6.
        assert!(bound_js("try { g(); } catch (e) { e(); }\n", "e"));
        assert!(!bound_js("try { g(); } catch (e) { g(); }\n", "g"));
    }

    #[test]
    fn loop_heads_bind_their_bodies() {
        // D7.
        assert!(bound_js("for (let i = 0; i < 3; i++) { i(); }\n", "i"));
        assert!(bound_js("for (const it of list) { it(); }\n", "it"));
        assert!(bound_js("for (var k in o) { k(); }\n", "k"));
        assert!(
            !bound_js("let k;\nfor (k of list) { k(); }\n", "k"),
            "assigning to an existing name declares nothing"
        );
    }

    #[test]
    fn a_named_function_expression_binds_its_own_name() {
        // D10: the name is visible inside the body only, so it is a local
        // and never a node.
        assert!(bound_js(
            "const fe = function named(){ named(); };\n",
            "named"
        ));
        assert!(
            !bound_js("function named(){}\nnamed();\n", "named"),
            "a function *declaration* is a node"
        );
    }

    #[test]
    fn arguments_is_a_local_binding_in_a_non_arrow_function() {
        // D14: an implicit binding, and not a node.
        assert!(bound_js("function f(){ arguments(); }\n", "arguments"));
        assert!(
            !bound_js("arguments();\n", "arguments"),
            "at module level there is no function to bind it"
        );
    }

    #[test]
    fn a_switch_case_block_binds() {
        assert!(bound_js(
            "function f(x){ switch (x) { case 1: { let s = 1; s(); } } }\n",
            "s"
        ));
    }

    #[test]
    fn a_class_body_is_not_a_local_binding_environment() {
        // Members are nodes; a call inside a method still names whatever the
        // module bound.
        assert!(!bound_js(
            "function helper(){}\nclass C { m(){ helper(); } }\n",
            "helper"
        ));
        assert!(bound_js(
            "function helper(){}\nclass C { m(helper){ helper(); } }\n",
            "helper"
        ));
    }

    #[test]
    fn a_namespace_body_is_not_a_local_binding_environment() {
        // C12/C14: namespace members are nodes, so the block that holds them
        // is a container and not a scope.
        assert!(!bound_ts(
            "namespace N { export const q = () => {}; q(); }\n",
            "q"
        ));
    }

    #[test]
    fn a_type_parameter_shadows_a_module_level_type() {
        // C23, the same class of bug as D5 one space over.
        assert!(bound_type(
            "type T = string;\nfunction f<T>(x: T): void {}\n",
            "T"
        ));
        assert!(!bound_type(
            "type T = string;\nfunction f(x: T): void {}\n",
            "T"
        ));
        assert!(bound_type(
            "type T = string;\nclass C<T> { m(x: T): void {} }\n",
            "T"
        ));
    }

    #[test]
    fn a_value_binding_does_not_bind_a_type_name_and_the_reverse() {
        // The two spaces are separate tables: `const T = 1` inside a function
        // says nothing about the type `T`.
        assert!(!bound_type(
            "type T = string;\nfunction f(): void { const T = 1; g<T>(); }\nlet z: T;\n",
            "T"
        ));
    }
}
