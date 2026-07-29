//! Names that are defined outside any repository: the ECMAScript, Node and
//! host globals, and the types TypeScript's own `lib` declares.
//!
//! Exactly Go's `GO_BUILTINS` → `External("go:builtin")` shape, split three
//! ways so the report is diagnostic: a corpus leaning on `document` is a
//! browser project and one leaning on `process` is a Node project, and one
//! bucket would say neither.
//!
//! **Consulted last, never first.** The universe scope is the outermost one,
//! so a module-level definition or an import of the same name wins — otherwise
//! a project that declares its own `URL` could never resolve it. The lists are
//! also deliberately conservative: `Node`, `Text`, `Element`, `Event` and
//! `Request` are host globals *and* ordinary names in real codebases, and
//! claiming them here would move real in-repository references into
//! `External`, which sits outside both terms of the rate. When in doubt a name
//! is left out, so the error is an honest miss rather than a silent exclusion.

/// ECMA-262 global object properties.
pub const ES_GLOBALS: &[&str] = &[
    "AggregateError",
    "Array",
    "ArrayBuffer",
    "Atomics",
    "BigInt",
    "BigInt64Array",
    "BigUint64Array",
    "Boolean",
    "DataView",
    "Date",
    "Error",
    "EvalError",
    "FinalizationRegistry",
    "Float32Array",
    "Float64Array",
    "Function",
    "Infinity",
    "Int16Array",
    "Int32Array",
    "Int8Array",
    "Intl",
    "JSON",
    "Map",
    "Math",
    "NaN",
    "Number",
    "Object",
    "Promise",
    "Proxy",
    "RangeError",
    "ReferenceError",
    "Reflect",
    "RegExp",
    "Set",
    "SharedArrayBuffer",
    "String",
    "Symbol",
    "SyntaxError",
    "TypeError",
    "URIError",
    "Uint16Array",
    "Uint32Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "WeakMap",
    "WeakRef",
    "WeakSet",
    "decodeURI",
    "decodeURIComponent",
    "encodeURI",
    "encodeURIComponent",
    "escape",
    "eval",
    "globalThis",
    "isFinite",
    "isNaN",
    "parseFloat",
    "parseInt",
    "undefined",
    "unescape",
];

/// Node-supplied globals, including the module wrapper's parameters.
pub const NODE_GLOBALS: &[&str] = &[
    "Buffer",
    "__dirname",
    "__filename",
    "clearImmediate",
    "exports",
    "global",
    "module",
    "process",
    "require",
    "setImmediate",
];

/// WHATWG / host globals common to browsers and modern Node.
pub const WEB_GLOBALS: &[&str] = &[
    "AbortController",
    "AbortSignal",
    "TextDecoder",
    "TextEncoder",
    "URL",
    "URLSearchParams",
    "WebSocket",
    "XMLHttpRequest",
    "atob",
    "btoa",
    "cancelAnimationFrame",
    "clearInterval",
    "clearTimeout",
    "console",
    "crypto",
    "document",
    "fetch",
    "localStorage",
    "navigator",
    "performance",
    "queueMicrotask",
    "requestAnimationFrame",
    "sessionStorage",
    "setInterval",
    "setTimeout",
    "structuredClone",
    "window",
];

/// Types TypeScript's bundled `lib` declares — utility types and the
/// well-known instance types. Type-space only.
///
/// Every one of these is a declaration in a `.d.ts` file this scan does not
/// index, which is what `External` means. Reporting them as unresolved would
/// fill TypeScript's denominator with the standard library.
pub const TS_LIB_TYPES: &[&str] = &[
    "Array",
    "Awaited",
    "Capitalize",
    "ConstructorParameters",
    "Exclude",
    "Extract",
    "InstanceType",
    "Iterable",
    "IterableIterator",
    "Iterator",
    "Lowercase",
    "Map",
    "NonNullable",
    "Omit",
    "OmitThisParameter",
    "Parameters",
    "Partial",
    "Pick",
    "Promise",
    "PromiseLike",
    "Readonly",
    "ReadonlyArray",
    "ReadonlyMap",
    "ReadonlySet",
    "Record",
    "Required",
    "ReturnType",
    "Set",
    "ThisParameterType",
    "ThisType",
    "Uncapitalize",
    "Uppercase",
    "WeakMap",
    "WeakSet",
];

/// An ambient environment a **package** injects into the universe scope.
///
/// The three lists above are the *host*'s. A name in them exists because the
/// runtime or the compiler's bundled `lib` declares it, and a reference to one
/// is [`crate::Outcome::External`]: a link to something outside the repository
/// that is genuinely there.
///
/// This is the other provenance, and it is the one the universe scope was
/// missing. A test runner's `describe`, `it` and `expect` are declared by a
/// package under `node_modules` — a package this scan does not index — and
/// reach the file without an import because the runner puts them there. The
/// honest reason is [`crate::UnresolvedReason::UnknownPackage`], and the proof
/// that it is honest is that this resolver already gives that exact answer for
/// the same definition whenever the import is written down: `import { expect }
/// from 'vitest'` in a package that does not vendor vitest reports
/// `UnknownPackage` today. The injected form names the same definition in the
/// same unindexed package, so it must not report something else — and what it
/// did report, [`crate::UnresolvedReason::NoMatchingDefinition`], claims the
/// lookup table was complete and the name absent, which is false on both
/// halves.
///
/// It is emphatically **not** `External`. That would move the reference out of
/// *both* terms of the resolution rate and raise the gate without linking
/// anything, which is the one way this gate can be cheated.
///
/// # Why the names are not gated on being rare
///
/// `it`, `test`, `before` and `expect` are ordinary words. What makes them
/// this environment's is not their spelling but the **project declaring the
/// package that injects them** — checked per file against the manifests, by
/// [`crate::track_ecma::project::EcmaConfig::declares_ambient`]. A repository
/// that depends on no test runner has nothing injecting `describe`, and there
/// the honest answer is still `NoMatchingDefinition`. So each list below is
/// the runner's own documented global set in full, rather than the subset of
/// it that looked unlikely to collide: a half-list would leave the same defect
/// behind under a different name, and a collision is answered by the scope
/// order anyway — the universe scope is consulted last, so any declaration or
/// import of the name wins.
pub struct Injected {
    /// What the environment is, for a reader of this table and for a test.
    pub env: &'static str,
    /// Declaring any one of these turns the environment on. More than one
    /// because a runner is named by its own package and by the `@types`
    /// package that describes it, and either is enough to say the globals are
    /// there.
    pub packages: &'static [&'static str],
    /// Every name the runner puts in the global scope.
    pub names: &'static [&'static str],
}

/// The ambient environments this build recognises.
///
/// Test runners only, so far, because that is the whole of the measured
/// class: every one of express's 1,728 `NoMatchingDefinition` occurrences and
/// 13,830 of vue-core's 15,276 are a runner's injected globals. A browser or
/// Node global belongs in [`WEB_GLOBALS`] or [`NODE_GLOBALS`] instead — the
/// host declares those, and `External` is what that means.
pub const INJECTED: &[Injected] = &[
    Injected {
        env: "mocha",
        packages: &["mocha", "@types/mocha"],
        names: &[
            "after",
            "afterEach",
            "before",
            "beforeEach",
            "context",
            "describe",
            "it",
            "specify",
            "xcontext",
            "xdescribe",
            "xit",
        ],
    },
    Injected {
        env: "jasmine",
        packages: &["jasmine", "jasmine-core", "@types/jasmine"],
        names: &[
            "afterAll",
            "afterEach",
            "beforeAll",
            "beforeEach",
            "describe",
            "expect",
            "fail",
            "fdescribe",
            "fit",
            "it",
            "jasmine",
            "pending",
            "spyOn",
            "spyOnAllFunctions",
            "spyOnProperty",
            "xdescribe",
            "xit",
        ],
    },
    Injected {
        env: "jest",
        packages: &["jest", "jest-cli", "babel-jest", "ts-jest", "@types/jest"],
        names: &[
            "afterAll",
            "afterEach",
            "beforeAll",
            "beforeEach",
            "describe",
            "expect",
            "fdescribe",
            "fit",
            "it",
            "jest",
            "test",
            "xdescribe",
            "xit",
            "xtest",
        ],
    },
    Injected {
        env: "vitest",
        packages: &["vitest", "@vitest/browser", "@vitest/ui"],
        names: &[
            "afterAll",
            "afterEach",
            "assert",
            "beforeAll",
            "beforeEach",
            "bench",
            "chai",
            "describe",
            "expect",
            "expectTypeOf",
            "it",
            "onTestFailed",
            "onTestFinished",
            "suite",
            "test",
            "vi",
            "vitest",
        ],
    },
    Injected {
        env: "cypress",
        packages: &["cypress"],
        names: &[
            "Cypress",
            "after",
            "afterEach",
            "before",
            "beforeEach",
            "context",
            "cy",
            "describe",
            "expect",
            "it",
            "specify",
            "xdescribe",
            "xit",
        ],
    },
    Injected {
        env: "qunit",
        packages: &["qunit", "@types/qunit"],
        names: &["QUnit"],
    },
];

/// The environment that injects `name` here, or `None`.
///
/// `declares` answers whether a package is present for the file being
/// resolved; the caller supplies it because this module reads no manifests.
/// Consulted **after** [`external_key`]: a name the host declares is the
/// host's, whatever a dependency also puts in scope.
pub fn injected_by(name: &str, declares: impl Fn(&str) -> bool) -> Option<&'static str> {
    INJECTED
        .iter()
        .find(|env| env.names.contains(&name) && env.packages.iter().any(|p| declares(p)))
        .map(|env| env.env)
}

/// The external key for a global name, or `None` when nothing declares it
/// outside the repository.
pub fn external_key(name: &str) -> Option<&'static str> {
    if ES_GLOBALS.contains(&name) {
        return Some("js:global");
    }
    if NODE_GLOBALS.contains(&name) {
        return Some("node:global");
    }
    if WEB_GLOBALS.contains(&name) {
        return Some("web:global");
    }
    None
}

/// The external key for a type-space name the TypeScript library declares.
pub fn lib_type_key(name: &str) -> Option<&'static str> {
    TS_LIB_TYPES.contains(&name).then_some("ts:lib")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_list_is_sorted_and_disjoint() {
        // Sorted so a reader can find a name, disjoint so `external_key` has
        // one answer rather than a precedence rule nobody wrote down.
        for list in [ES_GLOBALS, NODE_GLOBALS, WEB_GLOBALS, TS_LIB_TYPES] {
            let mut sorted = list.to_vec();
            sorted.sort_unstable();
            assert_eq!(list, sorted.as_slice());
        }
        for name in NODE_GLOBALS {
            assert!(!ES_GLOBALS.contains(name), "{name} is claimed twice");
            assert!(!WEB_GLOBALS.contains(name), "{name} is claimed twice");
        }
        for name in WEB_GLOBALS {
            assert!(!ES_GLOBALS.contains(name), "{name} is claimed twice");
        }
    }

    #[test]
    fn globals_classify_by_provenance() {
        assert_eq!(external_key("JSON"), Some("js:global"));
        assert_eq!(external_key("process"), Some("node:global"));
        assert_eq!(external_key("console"), Some("web:global"));
        assert_eq!(external_key("parse"), None);
    }

    #[test]
    fn the_error_constructors_are_present_as_a_family() {
        // `Error` was absent while all seven of its own subclasses were
        // listed — an omission, not the deliberate conservatism the module
        // header describes, which names host types real projects redeclare.
        // A missing global is not a harmless miss: it reports
        // `NoMatchingDefinition`, which says the lookup table was complete
        // and the name absent, and for `new Error(...)` neither half is true.
        for name in [
            "Error",
            "AggregateError",
            "EvalError",
            "RangeError",
            "ReferenceError",
            "SyntaxError",
            "TypeError",
            "URIError",
        ] {
            assert_eq!(external_key(name), Some("js:global"), "{name}");
        }
    }

    #[test]
    fn ambiguous_host_names_are_deliberately_absent() {
        // Real projects declare these. Claiming them would move in-repository
        // references into `External`, which is outside *both* terms of the
        // rate — the one way this gate can be raised without linking
        // anything.
        for name in ["Node", "Text", "Element", "Event", "Request", "Response"] {
            assert_eq!(external_key(name), None, "{name}");
        }
    }

    #[test]
    fn xmlhttprequest_is_a_host_global_like_the_rest_of_its_family() {
        // Absent while `WebSocket`, `AbortController` and `fetch` were all
        // listed — the same shape of omission as `Error`'s, and with the same
        // consequence: `NoMatchingDefinition` on a name no repository ever
        // declares, from a lookup table that was not complete.
        for name in ["XMLHttpRequest", "WebSocket", "fetch", "AbortController"] {
            assert_eq!(external_key(name), Some("web:global"), "{name}");
        }
    }

    #[test]
    fn every_injected_list_is_sorted_and_none_of_it_is_the_hosts() {
        // Sorted so a reader can find a name. Disjoint from the three host
        // lists so the two provenances can never disagree about one name:
        // `external_key` is consulted first, and a name in both would take the
        // `External` branch while this table claimed it was a package's.
        for env in INJECTED {
            let mut sorted = env.names.to_vec();
            sorted.sort_unstable();
            assert_eq!(env.names, sorted.as_slice(), "{}", env.env);
            assert!(!env.packages.is_empty(), "{} turns on for nothing", env.env);
            for name in env.names {
                assert_eq!(
                    external_key(name),
                    None,
                    "{name} is claimed by both the host and {}",
                    env.env,
                );
            }
        }
    }

    #[test]
    fn an_environment_is_a_package_and_not_a_word_list() {
        // The discriminator is the declared package, so the identical name
        // answers differently in two repositories — which is the whole point:
        // one of them really does have mocha putting `it` in scope and the
        // other really does have a missing definition.
        assert_eq!(injected_by("it", |p| p == "mocha"), Some("mocha"));
        assert_eq!(injected_by("it", |_| false), None);
        assert_eq!(injected_by("expect", |p| p == "vitest"), Some("vitest"));
        assert_eq!(injected_by("expect", |p| p == "mocha"), None);
        assert_eq!(injected_by("parseRoute", |_| true), None);
    }

    #[test]
    fn the_types_package_alone_turns_an_environment_on() {
        // A repository can carry `@types/mocha` without `mocha` — the runner
        // is invoked from somewhere else — and the globals are described all
        // the same.
        assert_eq!(
            injected_by("describe", |p| p == "@types/mocha"),
            Some("mocha")
        );
    }

    #[test]
    fn library_types_are_type_space_only() {
        assert_eq!(lib_type_key("Record"), Some("ts:lib"));
        assert_eq!(lib_type_key("Component"), None);
    }
}
