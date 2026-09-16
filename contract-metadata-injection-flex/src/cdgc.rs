// Copyright 2026 Salesforce, Inc. All rights reserved.
//! Pure CDGC helpers (no PDK imports) — fully unit-testable.
//!
//! The HTTP fetch itself lives in lib.rs (it needs the injected `HttpClient`);
//! everything deterministic — request-target encoding, the field-map path
//! resolver, and the cached-metadata types — lives here.

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Cached, extracted metadata for one asset: header name → stamped value.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct CachedMeta {
    pub fields: BTreeMap<String, String>,
    /// Unix seconds when fetched — drives the refresh TTL.
    pub timestamp: i64,
}

/// Single-initiator refresh lock entry (stampede control).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RefreshLock {
    pub acquired_at: i64,
}

/// Percent-encode a value for safe interpolation into a request path/query. PDK's request
/// builder takes the target verbatim and does no encoding, so an asset id containing
/// `?`, `#`, `/`, `&`, `=`, or whitespace could otherwise alter the target or inject params.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Per-request JWT nonce: nanoseconds since the Unix epoch, as a decimal string.
pub fn nonce_from_time(now: SystemTime) -> String {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// Resolve a "/"-separated path into a JSON value and return the leaf as a string.
///
/// Each segment is either an object key (which may itself be a dotted literal key such as
/// `core.score`, since we split on "/" not ".") or a numeric array index. Only scalar leaves
/// (string / number / bool) stamp cleanly into a header; objects/arrays/null → None.
pub fn resolve_path(root: &Value, path: &str) -> Option<String> {
    let mut cur = root;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Object(map) => map.get(seg)?,
            Value::Array(arr) => arr.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    match cur {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Parse the fieldMap JSON object (header → path) into ordered pairs. Invalid JSON → empty.
pub fn parse_field_map(json: &str) -> Vec<(String, String)> {
    match serde_json::from_str::<BTreeMap<String, String>>(json.trim()) {
        Ok(m) => m.into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

/// Extract every mapped field found in the asset-detail JSON. Missing paths are skipped.
pub fn build_fields(detail: &Value, field_map: &[(String, String)]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (header, path) in field_map {
        if let Some(v) = resolve_path(detail, path) {
            out.insert(header.clone(), v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn percent_encode_escapes_unsafe() {
        assert_eq!(percent_encode("abc-1.0_~"), "abc-1.0_~");
        assert_eq!(percent_encode("a b/c?d"), "a%20b%2Fc%3Fd");
    }

    #[test]
    fn resolve_nested_object() {
        let v = json!({"core":{"name":"Sales Orders"}});
        assert_eq!(resolve_path(&v, "core/name"), Some("Sales Orders".into()));
    }

    #[test]
    fn resolve_dotted_literal_key_in_array() {
        // CDGC uses literal dotted keys like "core.score"; splitting on "/" keeps them intact.
        let v = json!({"dataQuality":[{"core.score":92.5},{"core.score":80.0}]});
        assert_eq!(resolve_path(&v, "dataQuality/0/core.score"), Some("92.5".into()));
        assert_eq!(resolve_path(&v, "dataQuality/1/core.score"), Some("80.0".into()));
    }

    #[test]
    fn resolve_top_level_dotted_literal_key() {
        let v = json!({"core.classification":"Confidential"});
        assert_eq!(resolve_path(&v, "core.classification"), Some("Confidential".into()));
    }

    #[test]
    fn resolve_missing_and_complex_return_none() {
        let v = json!({"a":{"b":1},"arr":[1,2]});
        assert_eq!(resolve_path(&v, "a/missing"), None);
        assert_eq!(resolve_path(&v, "a"), None); // object leaf → not stampable
        assert_eq!(resolve_path(&v, "arr/9"), None); // out of range
        assert_eq!(resolve_path(&v, "arr"), None); // array leaf
    }

    #[test]
    fn field_map_parse_and_build() {
        let fm = parse_field_map(r#"{"x-dp-name":"core.name","x-dp-score":"dataQuality/0/core.score"}"#);
        assert_eq!(fm.len(), 2);
        let detail = json!({"core.name":"Orders","dataQuality":[{"core.score":91.0}]});
        let fields = build_fields(&detail, &fm);
        assert_eq!(fields.get("x-dp-name"), Some(&"Orders".to_string()));
        assert_eq!(fields.get("x-dp-score"), Some(&"91.0".to_string()));
    }

    #[test]
    fn field_map_bad_json_is_empty() {
        assert!(parse_field_map("not json").is_empty());
        assert!(parse_field_map("{}").is_empty());
    }

    #[test]
    fn build_fields_skips_missing_paths() {
        let fm = parse_field_map(r#"{"x-a":"a","x-missing":"nope/here"}"#);
        let fields = build_fields(&json!({"a":"1"}), &fm);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields.get("x-a"), Some(&"1".to_string()));
    }
}
