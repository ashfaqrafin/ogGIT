//! Canonical JSON (RFC 8785 subset). Signed documents must serialize to the
//! same bytes on every peer, so keys are sorted, there is no whitespace, and
//! floats are refused. The writer sorts keys itself instead of relying on
//! `serde_json`'s map type, which changes if any crate enables
//! `preserve_order`.

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum CanonicalError {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("floating point and out-of-range numbers are not allowed in signed documents")]
    Number,
    #[error("document is not in canonical form")]
    NotCanonical,
}

/// Largest integer that round-trips through an IEEE double, as RFC 8785 needs.
const MAX_SAFE_INT: u64 = (1 << 53) - 1;

pub fn to_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, CanonicalError> {
    let value = serde_json::to_value(value)?;
    let mut out = Vec::new();
    write(&value, &mut out)?;
    Ok(out)
}

/// Parses `bytes` and checks they are exactly the canonical encoding of the
/// result. This rejects unknown fields and alternative encodings, so the
/// bytes a signature covers are the only possible bytes for that document.
pub fn from_canonical<T: Serialize + DeserializeOwned>(bytes: &[u8]) -> Result<T, CanonicalError> {
    let value: T = serde_json::from_slice(bytes)?;
    if to_canonical(&value)? != bytes {
        return Err(CanonicalError::NotCanonical);
    }
    Ok(value)
}

fn write(value: &Value, out: &mut Vec<u8>) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => match (n.as_u64(), n.as_i64()) {
            (Some(u), _) if u <= MAX_SAFE_INT => out.extend_from_slice(u.to_string().as_bytes()),
            (None, Some(i)) if i.unsigned_abs() <= MAX_SAFE_INT => {
                out.extend_from_slice(i.to_string().as_bytes())
            }
            _ => return Err(CanonicalError::Number),
        },
        // serde_json escapes exactly what RFC 8785 requires: quote, backslash,
        // and control characters (short forms where they exist, else \u00xx).
        Value::String(s) => serde_json::to_writer(&mut *out, s)?,
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            // RFC 8785 orders keys by UTF-16 code units.
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push(b'{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key)?;
                out.push(b':');
                write(&map[key], out)?;
            }
            out.push(b'}');
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
        let v = json!({"b": 1, "a": [true, null, "x\ny"], "c": {"z": 0, "y": -3}});
        let out = to_canonical(&v).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"a":[true,null,"x\ny"],"b":1,"c":{"y":-3,"z":0}}"#
        );
    }

    #[test]
    fn refuses_floats() {
        assert!(to_canonical(&json!({"x": 1.5})).is_err());
        assert!(to_canonical(&json!({"x": u64::MAX})).is_err());
    }

    #[test]
    fn from_canonical_rejects_other_encodings() {
        let ok: Value = from_canonical(br#"{"a":1,"b":2}"#).unwrap();
        assert_eq!(ok, json!({"a": 1, "b": 2}));
        assert!(from_canonical::<Value>(br#"{"b":2,"a":1}"#).is_err());
        assert!(from_canonical::<Value>(br#"{"a": 1}"#).is_err());
    }
}
