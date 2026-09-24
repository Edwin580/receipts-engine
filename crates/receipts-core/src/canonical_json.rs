//! RFC 8785 (JSON Canonicalization Scheme) serialization, restricted to
//! integer numbers.
//!
//! Hashed JSON must not depend on key order or whitespace. Floats are
//! rejected rather than approximated: JCS formats them with ECMAScript's
//! `Number.prototype.toString` rules, and nothing hashed in M0 needs them.
//! Float support lands with plan hashing (M1), which has float literals.

use serde_json::Value;
use std::fmt;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NonIntegerNumber(pub String);

impl fmt::Display for NonIntegerNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "canonical JSON does not support non-integer number {}",
            self.0
        )
    }
}

impl std::error::Error for NonIntegerNumber {}

pub fn to_canonical_json(value: &Value) -> Result<String, NonIntegerNumber> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Ok(out)
}

fn write_value(value: &Value, out: &mut String) -> Result<(), NonIntegerNumber> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if !(n.is_i64() || n.is_u64()) {
                return Err(NonIntegerNumber(n.to_string()));
            }
            // |n| > 2^53 would lose precision in ECMAScript. Reject it too,
            // so a JavaScript verifier computes the same bytes.
            let fits = n.as_i64().is_some_and(|i| i.unsigned_abs() <= 1 << 53)
                || n.as_u64().is_some_and(|u| u <= 1 << 53);
            if !fits {
                return Err(NonIntegerNumber(n.to_string()));
            }
            out.push_str(&n.to_string());
        }
        // serde_json escapes exactly as JCS requires: `"` `\` and C0
        // controls, short forms for \b \f \n \r \t, lowercase \u00xx,
        // everything else literal.
        Value::String(s) => out.push_str(&Value::String(s.clone()).to_string()),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            // JCS orders keys by UTF-16 code units, not UTF-8 bytes.
            entries.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_value(item, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_strips_whitespace() {
        let v = json!({ "b": [1, { "z": null, "a": true }], "a": "x" });
        assert_eq!(
            to_canonical_json(&v).unwrap(),
            r#"{"a":"x","b":[1,{"a":true,"z":null}]}"#
        );
    }

    #[test]
    fn key_order_is_utf16() {
        // U+1F600 sorts after U+E000 in UTF-8, but before it in UTF-16
        // (its lead surrogate is 0xD83D).
        let v = json!({ "\u{1F600}": 1, "\u{E000}": 2 });
        assert_eq!(
            to_canonical_json(&v).unwrap(),
            "{\"\u{1F600}\":1,\"\u{E000}\":2}"
        );
    }

    #[test]
    fn string_escapes_match_jcs() {
        let v = json!("\u{8}\t\n\u{c}\r\"\\/\u{1f}é\u{2028}");
        assert_eq!(
            to_canonical_json(&v).unwrap(),
            "\"\\b\\t\\n\\f\\r\\\"\\\\/\\u001fé\u{2028}\""
        );
    }

    #[test]
    fn integers_only() {
        assert_eq!(
            to_canonical_json(&json!(-9007199254740992i64)).unwrap(),
            "-9007199254740992"
        );
        assert!(to_canonical_json(&json!(1.5)).is_err());
        assert!(to_canonical_json(&json!(9007199254740993u64)).is_err());
    }
}
