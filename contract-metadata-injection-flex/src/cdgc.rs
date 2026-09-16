// Copyright 2026 Salesforce, Inc. All rights reserved.
//! Pure CDGC helpers (no PDK imports) — the cached types + JWT nonce. The HTTP
//! calls live in lib.rs (they need the injected HttpClient).

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Cached, derived metadata for one asset: response header name → value.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
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

/// Per-request JWT nonce: nanoseconds since the Unix epoch as a decimal string.
pub fn nonce_from_time(now: SystemTime) -> String {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
