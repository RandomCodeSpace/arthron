//! A minimal JSON reader, sufficient for `package.json` and `tsconfig.json`.
//!
//! Written rather than depended on for two reasons. A `tsconfig.json` is not
//! JSON: it permits `//` and `/* */` comments and trailing commas, so a strict
//! parser rejects most real ones. And the two manifests between them use a
//! handful of keys — `name`, `type`, `main`, `module`, `exports`, `imports`,
//! plus tsconfig's `extends`/`compilerOptions`/`paths` — none of which needs a
//! derive, a schema, or a serializer.
//!
//! Deliberately lenient in one direction only: it accepts more than JSON
//! (comments, trailing commas), never less, and it never *guesses* — a
//! malformed document is `None`, which every caller treats as "this manifest
//! says nothing", not as "this manifest says the default".
//!
//! Numbers are kept as their source text. Nothing in module resolution does
//! arithmetic on a manifest value, and a lossless string cannot round a
//! version away.

use std::collections::BTreeMap;

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A number, as written.
    Number(String),
    /// A string, unescaped.
    String(String),
    /// An array, in source order.
    Array(Vec<Json>),
    /// An object. **Source order is preserved** in `order`: NODE
    /// `PACKAGE_TARGET_RESOLVE` matches conditional-export keys in object key
    /// order, so a map alone would silently reorder the resolution.
    Object {
        /// Keys in source order, duplicates removed (last write wins).
        order: Vec<String>,
        /// The entries.
        map: BTreeMap<String, Json>,
    },
}

impl Json {
    /// The string, when this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    /// The entries in source order, when this is an object.
    pub fn entries(&self) -> Vec<(&str, &Json)> {
        match self {
            Json::Object { order, map } => order
                .iter()
                .filter_map(|k| map.get(k).map(|v| (k.as_str(), v)))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// One member of an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object { map, .. } => map.get(key),
            _ => None,
        }
    }

    /// The elements, when this is an array.
    pub fn items(&self) -> &[Json] {
        match self {
            Json::Array(items) => items,
            _ => &[],
        }
    }
}

/// Parse a whole document. `None` when it is not one value plus whitespace.
pub fn parse(src: &str) -> Option<Json> {
    let bytes = src.as_bytes();
    let mut at = 0usize;
    skip_trivia(bytes, &mut at);
    let value = parse_value(bytes, &mut at)?;
    skip_trivia(bytes, &mut at);
    (at == bytes.len()).then_some(value)
}

/// Whitespace, `//` line comments and `/* */` block comments.
///
/// Comments are JSONC, not JSON. `tsconfig.json` is documented to allow them
/// and real ones use them heavily, so rejecting them would mean failing to
/// read the very configuration that decides `paths`.
fn skip_trivia(b: &[u8], at: &mut usize) {
    loop {
        while *at < b.len() && b[*at].is_ascii_whitespace() {
            *at += 1;
        }
        if b.len() >= *at + 2 && b[*at] == b'/' && b[*at + 1] == b'/' {
            while *at < b.len() && b[*at] != b'\n' {
                *at += 1;
            }
            continue;
        }
        if b.len() >= *at + 2 && b[*at] == b'/' && b[*at + 1] == b'*' {
            *at += 2;
            while *at < b.len() && !(b[*at] == b'*' && b.get(*at + 1) == Some(&b'/')) {
                *at += 1;
            }
            *at = (*at + 2).min(b.len());
            continue;
        }
        return;
    }
}

fn parse_value(b: &[u8], at: &mut usize) -> Option<Json> {
    match *b.get(*at)? {
        b'{' => parse_object(b, at),
        b'[' => parse_array(b, at),
        b'"' => parse_string(b, at).map(Json::String),
        b't' => literal(b, at, "true", Json::Bool(true)),
        b'f' => literal(b, at, "false", Json::Bool(false)),
        b'n' => literal(b, at, "null", Json::Null),
        _ => parse_number(b, at),
    }
}

fn literal(b: &[u8], at: &mut usize, word: &str, value: Json) -> Option<Json> {
    if b[*at..].starts_with(word.as_bytes()) {
        *at += word.len();
        return Some(value);
    }
    None
}

fn parse_number(b: &[u8], at: &mut usize) -> Option<Json> {
    let start = *at;
    if b.get(*at) == Some(&b'-') {
        *at += 1;
    }
    while b
        .get(*at)
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        *at += 1;
    }
    if *at == start {
        return None;
    }
    Some(Json::Number(String::from_utf8_lossy(&b[start..*at]).into()))
}

fn parse_string(b: &[u8], at: &mut usize) -> Option<String> {
    if b.get(*at) != Some(&b'"') {
        return None;
    }
    *at += 1;
    let mut out = String::new();
    loop {
        let c = *b.get(*at)?;
        *at += 1;
        match c {
            b'"' => return Some(out),
            b'\\' => {
                let e = *b.get(*at)?;
                *at += 1;
                match e {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let hex = b.get(*at..*at + 4)?;
                        *at += 4;
                        let code = u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                        // A lone surrogate is not a character. Replacing it
                        // keeps the parse total rather than dropping a whole
                        // manifest over one escape nothing reads.
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    }
                    _ => return None,
                }
            }
            _ => {
                // Multi-byte UTF-8 passes through whole: index back into the
                // original slice rather than pushing one byte at a time.
                let start = *at - 1;
                let len = utf8_len(c);
                let text = std::str::from_utf8(b.get(start..start + len)?).ok()?;
                out.push_str(text);
                *at = start + len;
            }
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn parse_array(b: &[u8], at: &mut usize) -> Option<Json> {
    *at += 1; // '['
    let mut items = Vec::new();
    loop {
        skip_trivia(b, at);
        if b.get(*at) == Some(&b']') {
            *at += 1;
            return Some(Json::Array(items));
        }
        items.push(parse_value(b, at)?);
        skip_trivia(b, at);
        match b.get(*at) {
            Some(b',') => *at += 1,
            Some(b']') => {}
            _ => return None,
        }
    }
}

fn parse_object(b: &[u8], at: &mut usize) -> Option<Json> {
    *at += 1; // '{'
    let mut order: Vec<String> = Vec::new();
    let mut map: BTreeMap<String, Json> = BTreeMap::new();
    loop {
        skip_trivia(b, at);
        if b.get(*at) == Some(&b'}') {
            *at += 1;
            return Some(Json::Object { order, map });
        }
        let key = parse_string(b, at)?;
        skip_trivia(b, at);
        if b.get(*at) != Some(&b':') {
            return None;
        }
        *at += 1;
        skip_trivia(b, at);
        let value = parse_value(b, at)?;
        if map.insert(key.clone(), value).is_none() {
            order.push(key);
        }
        skip_trivia(b, at);
        match b.get(*at) {
            Some(b',') => *at += 1,
            Some(b'}') => {}
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_package_json() {
        let v = parse(r#"{"name":"pkg","type":"module","main":"./lib/i.js"}"#).expect("parses");
        assert_eq!(v.get("name").and_then(Json::as_str), Some("pkg"));
        assert_eq!(v.get("type").and_then(Json::as_str), Some("module"));
        assert_eq!(v.get("missing"), None);
    }

    #[test]
    fn object_key_order_survives() {
        // NODE `PACKAGE_TARGET_RESOLVE` matches conditions in object key
        // order, so this is a resolution input and not a formatting detail.
        let v = parse(r#"{"types":"a","import":"b","require":"c","default":"d"}"#).expect("parses");
        let keys: Vec<&str> = v.entries().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["types", "import", "require", "default"]);
    }

    #[test]
    fn tsconfig_comments_and_trailing_commas_are_accepted() {
        let src = r#"{
            // the base config
            "extends": "./tsconfig.base.json",
            "compilerOptions": {
                /* path aliases */
                "baseUrl": ".",
                "paths": { "@vue/*": ["packages/*/src"], },
            },
        }"#;
        let v = parse(src).expect("jsonc parses");
        assert_eq!(
            v.get("extends").and_then(Json::as_str),
            Some("./tsconfig.base.json")
        );
        let paths = v
            .get("compilerOptions")
            .and_then(|c| c.get("paths"))
            .expect("paths");
        assert_eq!(
            paths.get("@vue/*").map(|t| t.items().len()),
            Some(1),
            "one substitution"
        );
    }

    #[test]
    fn escapes_and_unicode_round_trip() {
        let v = parse(r#"{"a":"x\nyéz\"q\\","b":"héllo"}"#).expect("parses");
        assert_eq!(v.get("a").and_then(Json::as_str), Some("x\nyéz\"q\\"));
        assert_eq!(v.get("b").and_then(Json::as_str), Some("héllo"));
    }

    #[test]
    fn arrays_numbers_and_literals() {
        let v = parse(r#"[1, -2.5e3, true, false, null, "s"]"#).expect("parses");
        assert_eq!(v.items().len(), 6);
        assert_eq!(v.items()[0], Json::Number("1".into()));
        assert_eq!(v.items()[1], Json::Number("-2.5e3".into()));
        assert_eq!(v.items()[2], Json::Bool(true));
        assert_eq!(v.items()[4], Json::Null);
    }

    #[test]
    fn a_malformed_document_says_nothing_rather_than_guessing() {
        // Every caller reads `None` as "this manifest states no opinion".
        // Returning a partial object here would let half a truncated
        // package.json decide a module kind.
        assert_eq!(parse("{"), None);
        assert_eq!(parse(r#"{"a" 1}"#), None);
        assert_eq!(parse(r#"{"a":1} trailing"#), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn a_duplicate_key_keeps_one_position_and_the_last_value() {
        let v = parse(r#"{"a":1,"b":2,"a":3}"#).expect("parses");
        let keys: Vec<&str> = v.entries().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["a", "b"]);
        assert_eq!(v.get("a"), Some(&Json::Number("3".into())));
    }
}
