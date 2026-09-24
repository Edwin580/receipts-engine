//! RFC 8785 (JSON Canonicalization Scheme) serialization.
//!
//! Hashed JSON must not depend on key order or whitespace. Numbers are
//! written the way ECMAScript's `Number.prototype.toString` writes them
//! (JCS §3.2.2.3), so a JavaScript verifier computes the same bytes:
//! integers up to 2^53 in magnitude, and finite floats (M1, for plan
//! literals). Larger integers are rejected because they would round in
//! ECMAScript; NaN and infinities cannot occur in JSON.

use serde_json::Value;
use std::fmt;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnrepresentableNumber(pub String);

impl fmt::Display for UnrepresentableNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "canonical JSON cannot represent {} exactly (integer beyond 2^53)",
            self.0
        )
    }
}

impl std::error::Error for UnrepresentableNumber {}

pub fn to_canonical_json(value: &Value) -> Result<String, UnrepresentableNumber> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Ok(out)
}

fn write_value(value: &Value, out: &mut String) -> Result<(), UnrepresentableNumber> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(f) = n.as_f64().filter(|_| n.is_f64()) {
                out.push_str(&format_es_number(f));
                return Ok(());
            }
            // |n| > 2^53 would lose precision in ECMAScript. Reject it too,
            // so a JavaScript verifier computes the same bytes.
            let fits = n.as_i64().is_some_and(|i| i.unsigned_abs() <= 1 << 53)
                || n.as_u64().is_some_and(|u| u <= 1 << 53);
            if !fits {
                return Err(UnrepresentableNumber(n.to_string()));
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

/// Formats a finite `f64` exactly as ECMAScript `Number.prototype.toString`
/// does (ECMA-262 §6.1.6.1.20): the shortest digits that round-trip, in
/// plain notation for decimal exponents in `-6 < n <= 21`, else exponent
/// notation such as `1e+21` or `1.5e-7`. `-0` is written as `0`.
///
/// # Panics
/// If `x` is NaN or infinite.
pub fn format_es_number(x: f64) -> String {
    assert!(x.is_finite(), "{x} has no JSON representation");
    if x == 0.0 {
        return "0".into();
    }
    // Rust's `{:e}` also prints the shortest round-trip digits, as
    // `d[.ddd]e<exp>`.
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp has an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    // x = 0.d1d2..dk × 10^n
    let n = exp.parse::<i32>().expect("LowerExp exponent is an integer") + 1;
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = n - 1;
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&e.unsigned_abs().to_string());
    }
    out
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
    fn integers_up_to_2_pow_53() {
        assert_eq!(
            to_canonical_json(&json!(-9007199254740992i64)).unwrap(),
            "-9007199254740992"
        );
        assert!(to_canonical_json(&json!(9007199254740993u64)).is_err());
    }

    #[test]
    fn floats_use_ecmascript_formatting() {
        // Expected strings are what `String(x)` prints in V8/SpiderMonkey,
        // including the JCS appendix B examples.
        let cases: &[(f64, &str)] = &[
            (1.5, "1.5"),
            (-1.5, "-1.5"),
            (1.0, "1"),
            (-0.0, "0"),
            (0.1, "0.1"),
            (123.456, "123.456"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (0.000001, "0.000001"),
            (1e-7, "1e-7"),
            (1.25e-7, "1.25e-7"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (4.5, "4.5"),
            (0.002, "0.002"),
            (0.000001234, "0.000001234"),
            (333333333.3333333, "333333333.3333333"),
            (9007199254740994.0, "9007199254740994"),
            (295147905179352830000.0, "295147905179352830000"),
            (1e23, "1e+23"),
            (-5e-7, "-5e-7"),
        ];
        for (x, want) in cases {
            assert_eq!(format_es_number(*x), *want, "{x:e}");
            assert_eq!(to_canonical_json(&json!(x)).unwrap(), *want);
        }
    }

    proptest::proptest! {
        #[test]
        fn floats_round_trip(bits in proptest::prelude::any::<u64>()) {
            let x = f64::from_bits(bits);
            proptest::prop_assume!(x.is_finite());
            let s = format_es_number(x);
            let back: f64 = s.parse().unwrap();
            proptest::prop_assert_eq!(back, if x == 0.0 { 0.0 } else { x });
        }
    }
}
