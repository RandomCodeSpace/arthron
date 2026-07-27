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
    fn library_types_are_type_space_only() {
        assert_eq!(lib_type_key("Record"), Some("ts:lib"));
        assert_eq!(lib_type_key("Component"), None);
    }
}
