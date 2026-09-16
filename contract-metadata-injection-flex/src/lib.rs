// Copyright 2026 Salesforce, Inc. All rights reserved.
//! Data Product Contract Metadata Injection — inbound, response-header-only,
//! fail-open Omni/Flex Gateway policy. Works on MCP, A2A and REST/HTTP.
//!
//! Fetches a CDGC-governed asset's metadata from Informatica IDMC (Login → JWT →
//! data360 asset detail), extracts a configurable set of fields, and stamps them
//! onto the response as headers (name, version, owner, classification, SLA,
//! purposes, …) so the data product's output port is self-describing and every
//! downstream governance policy can read cheap headers instead of its own catalog
//! lookup. The score/metadata is cached in PDK DataStorage and refreshed lazily
//! (single-flight), so almost every request is a fast local read.
//!
//! Connectivity: the two CDGC hosts are `format: service` config properties, which
//! the generated `init()` registers as outbound clusters (`service_create`); the
//! injected `HttpClient` dispatches to them. Credentials are `security:sensitive`.
//!
//! Enrichment, **fail-open**: any CDGC error degrades to serving last-known-good
//! (or an `x-dp-metadata-status: unavailable` marker) — it never blocks or errors
//! the response. Headers only (no body read) → safe for SSE / token streaming.

mod cdgc;
mod generated;

use std::rc::Rc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use pdk::data_storage::{DataStorage, DataStorageBuilder, DataStorageError, StoreMode};
use pdk::hl::timer::Clock;
use pdk::hl::*;
use pdk::logger;
use serde::Deserialize;
use serde_json::Value;

use crate::cdgc::{
    build_fields, nonce_from_time, parse_field_map, percent_encode, CachedMeta, RefreshLock,
};
use crate::generated::config::Config;

const META_CACHE_NAMESPACE: &str = "cmi-metadata";
const REFRESH_LOCK_NAMESPACE: &str = "cmi-refresh-lock";
const META_CACHE_KEY_PREFIX: &str = "cmi-meta-";
const REFRESH_LOCK_KEY_PREFIX: &str = "cmi-lock-";
const REFRESH_LOCK_TTL_SECONDS: i64 = 30;
const REFRESH_LOCK_TTL_MS: u32 = (REFRESH_LOCK_TTL_SECONDS as u32) * 1000;
const META_STORE_MIN_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000; // 30 days
const CAS_MAX_RETRIES: u32 = 3;
const DEFAULT_TIMEOUT_MS: i64 = 5_000;
const CDGC_REFRESH_BUDGET_MS: i64 = 10_000;
const DEFAULT_REFRESH_INTERVAL_SECONDS: i64 = 86_400;
const DEFAULT_FIELD_MAP: &str = r#"{"x-dp-name":"summary/core.name","x-dp-description":"summary/core.description","x-dp-external-id":"core.externalId","x-dp-dq-score":"dataQuality/0/core.score"}"#;
const DEFAULT_SEGMENTS: &str = "core,summary,dataQuality";

#[derive(Deserialize)]
struct CdgcLoginResponse {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "orgId")]
    org_id: String,
}

#[derive(Deserialize)]
struct CdgcJwtResponse {
    jwt_token: String,
}

/// Threaded request → response: the asset id to enrich this response with.
#[derive(Clone)]
struct Ctx {
    asset_id: String,
}

/// JSON-RPC methods that carry data-product content worth enriching. Everything
/// else (MCP initialize/tools-list/notifications/ping, A2A housekeeping) is skipped.
fn is_content_method(method: &str) -> bool {
    matches!(
        method,
        "tools/call" | "resources/read" | "prompts/get"
            | "message/send" | "message/stream" | "SendMessage" | "SendStreamingMessage"
    )
}

fn now_secs(clock: &Clock) -> i64 {
    clock.now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn elapsed_ms(start: SystemTime, now: SystemTime) -> i64 {
    now.duration_since(start).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Per-call timeout clamped to the overall refresh budget still remaining.
fn next_call_timeout(per_call_ms: i64, elapsed: i64) -> Option<Duration> {
    let remaining = CDGC_REFRESH_BUDGET_MS - elapsed;
    if remaining <= 0 {
        return None;
    }
    Some(Duration::from_millis(per_call_ms.min(remaining).max(1) as u64))
}

fn meta_store_ttl_ms(config: &Config) -> u32 {
    let refresh = config.refresh_interval_seconds.unwrap_or(DEFAULT_REFRESH_INTERVAL_SECONDS).max(0) as u64;
    refresh.saturating_mul(2).saturating_mul(1000).max(META_STORE_MIN_TTL_MS).min(u32::MAX as u64) as u32
}

/// Login → JWT → data360 asset Detail, then extract the configured fields. Runs only
/// on a cache miss/stale entry; bounded by the overall refresh budget.
async fn fetch_cdgc_metadata(
    client: &HttpClient,
    config: &Config,
    clock: &Clock,
    asset_id: &str,
) -> Result<std::collections::BTreeMap<String, String>> {
    let start = clock.now();
    let per_call = config.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);

    // 1. Login
    let login_body = serde_json::to_vec(&serde_json::json!({
        "username": config.cdgc_org_username, "password": config.cdgc_org_password,
    }))?;
    let t = next_call_timeout(per_call, elapsed_ms(start, clock.now()))
        .ok_or_else(|| anyhow!("CDGC refresh exceeded budget before Login"))?;
    let login_resp = client
        .request(&config.cdgc_login_url)
        .path("/identity-service/api/v1/Login")
        .headers(vec![("Content-Type", "application/json")])
        .body(&login_body)
        .timeout(t)
        .post()
        .await
        .map_err(|e| anyhow!("CDGC login failed: {e}"))?;
    if login_resp.status_code() >= 300 {
        return Err(anyhow!("CDGC login returned status {}", login_resp.status_code()));
    }
    let login: CdgcLoginResponse = serde_json::from_slice(login_resp.body())
        .map_err(|e| anyhow!("Failed to parse CDGC login response: {e}"))?;

    // 2. JWT
    let nonce = percent_encode(&nonce_from_time(clock.now()));
    let jwt_path = format!("/identity-service/api/v1/jwt/Token?client_id=idmc_api&nonce={nonce}");
    let cookie = format!("USER_SESSION={}", login.session_id);
    let t = next_call_timeout(per_call, elapsed_ms(start, clock.now()))
        .ok_or_else(|| anyhow!("CDGC refresh exceeded budget before JWT"))?;
    let jwt_resp = client
        .request(&config.cdgc_login_url)
        .path(&jwt_path)
        .headers(vec![("cookie", cookie.as_str()), ("IDS-SESSION-ID", login.session_id.as_str())])
        .timeout(t)
        .get()
        .await
        .map_err(|e| anyhow!("CDGC JWT fetch failed: {e}"))?;
    if jwt_resp.status_code() >= 300 {
        return Err(anyhow!("CDGC JWT fetch returned status {}", jwt_resp.status_code()));
    }
    let jwt: CdgcJwtResponse = serde_json::from_slice(jwt_resp.body())
        .map_err(|e| anyhow!("Failed to parse CDGC JWT response: {e}"))?;

    // 3. Asset detail
    // segments is a controlled, comma-separated config value — passed raw so the
    // comma keeps its list semantics (percent-encoding it to %2C breaks the API).
    let segments = config.segments.as_deref().unwrap_or(DEFAULT_SEGMENTS);
    let detail_path = format!(
        "/data360/search/v1/assets/{}?scheme=internal&segments={}",
        percent_encode(asset_id),
        segments
    );
    let authorization = format!("Bearer {}", jwt.jwt_token);
    let t = next_call_timeout(per_call, elapsed_ms(start, clock.now()))
        .ok_or_else(|| anyhow!("CDGC refresh exceeded budget before Detail"))?;
    let detail_resp = client
        .request(&config.cdgc_base_api_url)
        .path(&detail_path)
        .headers(vec![
            ("Authorization", authorization.as_str()),
            ("X-INFA-ORG-ID", login.org_id.as_str()),
            ("Content-Type", "application/json"),
        ])
        .timeout(t)
        .get()
        .await
        .map_err(|e| anyhow!("CDGC asset detail fetch failed: {e}"))?;
    if detail_resp.status_code() >= 300 {
        return Err(anyhow!("CDGC asset detail returned status {}", detail_resp.status_code()));
    }
    let detail: Value = serde_json::from_slice(detail_resp.body())
        .map_err(|e| anyhow!("Failed to parse CDGC asset detail: {e}"))?;

    let field_map = parse_field_map(config.field_map.as_deref().unwrap_or(DEFAULT_FIELD_MAP));
    Ok(build_fields(&detail, &field_map))
}

async fn read_cached<S: DataStorage>(store: &S, key: &str) -> Option<CachedMeta> {
    match store.get::<CachedMeta>(key).await {
        Ok(Some((cached, _v))) => Some(cached),
        Ok(None) => None,
        Err(e) => {
            logger::warn!("cmi: failed to read cached metadata: {e}");
            None
        }
    }
}

/// Gossip-safe write: Absent on a miss, Cas overwrite on an existing entry; never DELETE.
async fn write_cached<S: DataStorage>(store: &S, key: &str, entry: &CachedMeta) {
    for _ in 0..CAS_MAX_RETRIES {
        match store.get::<CachedMeta>(key).await {
            Ok(Some((_, version))) => match store.store(key, &StoreMode::Cas(version), entry).await {
                Ok(()) => return,
                Err(DataStorageError::CasMismatch) => continue,
                Err(e) => {
                    logger::warn!("cmi: failed to persist metadata: {e}");
                    return;
                }
            },
            Ok(None) => match store.store(key, &StoreMode::Absent, entry).await {
                Ok(()) => return,
                Err(DataStorageError::CasMismatch) => continue,
                Err(e) => {
                    logger::warn!("cmi: failed to persist metadata: {e}");
                    return;
                }
            },
            Err(e) => {
                logger::warn!("cmi: failed to read before persist: {e}");
                return;
            }
        }
    }
}

/// Single-initiator refresh lock (atomic put-if-absent; stale holder taken over via CAS).
async fn try_acquire_refresh_lock<S: DataStorage>(store: &S, key: &str, now: i64) -> Result<bool, DataStorageError> {
    let entry = RefreshLock { acquired_at: now };
    match store.store(key, &StoreMode::Absent, &entry).await {
        Ok(()) => Ok(true),
        Err(DataStorageError::CasMismatch) => match store.get::<RefreshLock>(key).await? {
            Some((existing, version)) => {
                if now - existing.acquired_at < REFRESH_LOCK_TTL_SECONDS {
                    Ok(false)
                } else {
                    match store.store(key, &StoreMode::Cas(version), &entry).await {
                        Ok(()) => Ok(true),
                        Err(DataStorageError::CasMismatch) => Ok(false),
                        Err(e) => Err(e),
                    }
                }
            }
            None => match store.store(key, &StoreMode::Absent, &entry).await {
                Ok(()) => Ok(true),
                Err(DataStorageError::CasMismatch) => Ok(false),
                Err(e) => Err(e),
            },
        },
        Err(e) => Err(e),
    }
}

/// Cache-aside: fresh cache → serve; else single-flight refresh from CDGC; on error, fail-open
/// to last-known-good (when configured). Returns the header→value fields, or None.
#[allow(clippy::too_many_arguments)]
async fn get_metadata<S: DataStorage>(
    client: &HttpClient,
    config: &Config,
    clock: &Clock,
    meta_store: &S,
    lock_store: &S,
    asset_id: &str,
) -> Option<std::collections::BTreeMap<String, String>> {
    let key = format!("{META_CACHE_KEY_PREFIX}{asset_id}");
    let ttl = config.refresh_interval_seconds.unwrap_or(DEFAULT_REFRESH_INTERVAL_SECONDS).max(0);
    let now = now_secs(clock);

    let cached = read_cached(meta_store, &key).await;
    if let Some(c) = &cached {
        if now - c.timestamp < ttl {
            return Some(c.fields.clone());
        }
    }

    // Stale or missing → single-flight refresh.
    let lock_key = format!("{REFRESH_LOCK_KEY_PREFIX}{asset_id}");
    let acquired = try_acquire_refresh_lock(lock_store, &lock_key, now).await.unwrap_or(true);
    if !acquired {
        return cached.map(|c| c.fields); // another worker is refreshing; serve stale if any
    }

    match fetch_cdgc_metadata(client, config, clock, asset_id).await {
        Ok(fields) => {
            write_cached(meta_store, &key, &CachedMeta { fields: fields.clone(), timestamp: now }).await;
            Some(fields)
        }
        Err(e) => {
            logger::warn!("cmi: CDGC metadata refresh failed for '{asset_id}': {e}");
            if config.fail_open_on_cdgc_error.unwrap_or(true) {
                cached.map(|c| c.fields)
            } else {
                None
            }
        }
    }
}

async fn request_filter(request_state: RequestState, config: Rc<Config>) -> Flow<Option<Ctx>> {
    let hs = request_state.into_headers_state().await;
    let header_name = config.asset_id_header.as_deref().unwrap_or("x-dp-contract-id");
    let asset_id = hs
        .handler()
        .header(header_name)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| config.cdgc_asset_id.clone());

    // Content detection: skip known non-content JSON-RPC methods; enrich REST + content methods.
    let ct = hs.handler().header("content-type").unwrap_or_default();
    if ct.starts_with("application/json") && hs.method().as_str() == "POST" {
        let bs = hs.into_body_state().await;
        if let Ok(v) = serde_json::from_slice::<Value>(&bs.handler().body()) {
            if let Some(method) = v.get("method").and_then(Value::as_str) {
                if !is_content_method(method) {
                    return Flow::Continue(None);
                }
            }
        }
    }
    Flow::Continue(Some(Ctx { asset_id }))
}

#[allow(clippy::too_many_arguments)]
async fn response_filter<S: DataStorage>(
    response_state: ResponseState,
    request_data: RequestData<Option<Ctx>>,
    config: Rc<Config>,
    client: Rc<HttpClient>,
    clock: Rc<Clock>,
    meta_store: Rc<S>,
    lock_store: Rc<S>,
) {
    let asset_id = match request_data {
        RequestData::Continue(Some(ctx)) => ctx.asset_id,
        _ => return,
    };
    let hs = response_state.into_headers_state().await;
    let h = hs.handler();

    match get_metadata(&client, &config, &clock, &*meta_store, &*lock_store, &asset_id).await {
        Some(fields) if !fields.is_empty() => {
            for (name, value) in &fields {
                h.set_header(name, value);
            }
            h.set_header("x-dp-asset-id", &asset_id);
            h.set_header("x-dp-metadata-source", "cdgc");
            h.set_header("x-dp-metadata-status", "ok");
            logger::info!("cmi: stamped {} metadata field(s) for asset '{asset_id}'", fields.len());
        }
        _ => {
            if config.header_on_miss.unwrap_or(true) {
                h.set_header("x-dp-asset-id", &asset_id);
                h.set_header("x-dp-metadata-status", "unavailable");
            }
        }
    }
}

fn launch_policy<S: DataStorage + 'static>(
    launcher: Launcher,
    config: Rc<Config>,
    client: Rc<HttpClient>,
    clock: Rc<Clock>,
    meta_store: Rc<S>,
    lock_store: Rc<S>,
) -> impl std::future::Future<Output = Result<()>> {
    let cfg_req = config.clone();
    let filter = on_request(move |rs| {
        let c = cfg_req.clone();
        async move { request_filter(rs, c).await }
    })
    .on_response(move |rs, rd| {
        let c = config.clone();
        let cl = client.clone();
        let ck = clock.clone();
        let ms = meta_store.clone();
        let ls = lock_store.clone();
        async move { response_filter(rs, rd, c, cl, ck, ms, ls).await }
    });
    async move { launcher.launch(filter).await.map_err(Into::into) }
}

#[entrypoint]
async fn configure(
    launcher: Launcher,
    Configuration(bytes): Configuration,
    client: HttpClient,
    storage_builder: DataStorageBuilder,
    clock: Clock,
) -> Result<()> {
    let config: Config = serde_json::from_slice(&bytes).map_err(|err| {
        anyhow!("Failed to parse configuration '{}'. Cause: {}", String::from_utf8_lossy(&bytes), err)
    })?;
    let config = Rc::new(config);
    let client = Rc::new(client);
    let clock = Rc::new(clock);

    if config.distributed.unwrap_or(false) {
        let meta = Rc::new(storage_builder.remote(META_CACHE_NAMESPACE, meta_store_ttl_ms(&config)));
        let lock = Rc::new(storage_builder.remote(REFRESH_LOCK_NAMESPACE, REFRESH_LOCK_TTL_MS));
        launch_policy(launcher, config, client, clock, meta, lock).await
    } else {
        let meta = Rc::new(storage_builder.local(META_CACHE_NAMESPACE));
        let lock = Rc::new(storage_builder.local(REFRESH_LOCK_NAMESPACE));
        launch_policy(launcher, config, client, clock, meta, lock).await
    }
}
