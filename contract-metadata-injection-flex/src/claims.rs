// Copyright 2026 Salesforce, Inc. All rights reserved.
//! Read caller attributes from a validated JWT's claims.
//!
//! A gateway bearer token is `header.PAYLOAD.signature`; the payload is
//! base64url-encoded JSON. This module **decodes** (it does NOT verify) that
//! payload and reads named claims. Signature/expiry validation is delegated to
//! an upstream **JWT Validation policy** on the same API instance — this reads
//! only already-trusted claims. Without such an upstream policy the claims are
//! attacker-controlled and MUST NOT be trusted; that is why the JWT-claims
//! source is opt-in (a claim name must be configured) and the header source
//! remains the default.

use serde_json::Value;

/// Decode the unverified claims payload of an `Authorization: Bearer <jwt>` header.
/// Returns `None` when the header is missing, not a Bearer token, or malformed.
pub fn decode_bearer_claims(authorization: Option<&str>) -> Option<Value> {
    let raw = authorization?.trim();
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .or_else(|| raw.strip_prefix("BEARER "))?
        .trim();
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

/// A claim's value as a single trimmed, non-empty string.
///
/// `name` is matched as a literal top-level key, so namespaced claims such as
/// `https://example.com/clearance` work as written. A string is used as-is; a
/// single-element array yields that element; a number is stringified. Multi-element
/// arrays and objects return `None` — clearance/purpose/schema-id are expected to
/// be single-valued claims.
pub fn claim_str(claims: &Value, name: &str) -> Option<String> {
    let v = claims.get(name)?;
    let s = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) if a.len() == 1 => a[0].as_str()?.trim().to_string(),
        _ => return None,
    };
    (!s.is_empty()).then_some(s)
}

/// Minimal base64url (RFC 4648 §5, no padding) decoder — no external crate.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn six(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &c in input.as_bytes() {
        if c == b'=' {
            break;
        }
        buf = (buf << 6) | six(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b64url(bytes: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut s = String::new();
        for chunk in bytes.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            for i in 0..chunk.len() + 1 {
                s.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        s
    }
    fn make_jwt(payload: &Value) -> String {
        format!("h.{}.sig", b64url(payload.to_string().as_bytes()))
    }

    #[test]
    fn decodes_and_reads_claims() {
        let jwt = make_jwt(&json!({"clearance":"restricted","purpose":"fraud-detection"}));
        let auth = format!("Bearer {jwt}");
        let claims = decode_bearer_claims(Some(&auth)).unwrap();
        assert_eq!(claim_str(&claims, "clearance").as_deref(), Some("restricted"));
        assert_eq!(claim_str(&claims, "purpose").as_deref(), Some("fraud-detection"));
        assert_eq!(claim_str(&claims, "missing"), None);
    }

    #[test]
    fn namespaced_claim_key_and_lowercase_scheme() {
        let jwt = make_jwt(&json!({"https://acme/clearance":"internal"}));
        let auth = format!("bearer {jwt}");
        let claims = decode_bearer_claims(Some(&auth)).unwrap();
        assert_eq!(claim_str(&claims, "https://acme/clearance").as_deref(), Some("internal"));
    }

    #[test]
    fn single_element_array_and_number() {
        let claims = json!({"roles":["restricted"],"lvl":3,"many":["a","b"]});
        assert_eq!(claim_str(&claims, "roles").as_deref(), Some("restricted"));
        assert_eq!(claim_str(&claims, "lvl").as_deref(), Some("3"));
        assert_eq!(claim_str(&claims, "many"), None);
    }

    #[test]
    fn non_bearer_or_malformed() {
        assert!(decode_bearer_claims(None).is_none());
        assert!(decode_bearer_claims(Some("Basic xyz")).is_none());
        assert!(decode_bearer_claims(Some("Bearer notajwt")).is_none());
    }
}
