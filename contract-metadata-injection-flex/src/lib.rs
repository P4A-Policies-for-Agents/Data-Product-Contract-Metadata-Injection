// Copyright 2026 Salesforce, Inc. All rights reserved.
//! Data Product Contract Metadata Injection — inbound, headers-only, fail-open
//! Omni/Flex Gateway policy. Works on MCP, A2A and REST/HTTP.
//!
//! Given only a CDGC schema-asset id (a scanned flat file, table, etc.), it derives
//! the data product's governed identity from Informatica CDGC (via the ccgf-searchv2
//! search API): the asset's name/external-id, its columns, and the required/sensitive
//! flags from the columns' linked Business Terms. Sensitivity comes from the term's
//! structured Security Level (securityClassification) ∈ sensitiveLevels, falling back
//! to the description sensitiveMarker only when no level is set. It stamps that summary
//! onto the response as x-dp-* headers so the output port is self-describing.
//!
//! The CDGC fetch runs on the REQUEST leg (await-safe under enable_stop_iteration)
//! and is stamped synchronously on the response leg — avoiding the response-leg
//! fetch racing the streamed response-head commit. Cached (lazy refresh,
//! single-flight, distributed opt-in). Enrichment, fail-open: never blocks.

mod cdgc;
mod claims;
mod generated;

use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use pdk::data_storage::{DataStorage, DataStorageBuilder, DataStorageError, StoreMode};
use pdk::hl::timer::Clock;
use pdk::hl::*;
use pdk::logger;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cdgc::{nonce_from_time, CachedMeta, RefreshLock};
use crate::generated::config::Config;

const META_CACHE_NAMESPACE: &str = "cmi-metadata";
const REFRESH_LOCK_NAMESPACE: &str = "cmi-refresh-lock";
const META_CACHE_KEY_PREFIX: &str = "cmi-meta-";
const REFRESH_LOCK_KEY_PREFIX: &str = "cmi-lock-";
const REFRESH_LOCK_TTL_SECONDS: i64 = 30;
const REFRESH_LOCK_TTL_MS: u32 = (REFRESH_LOCK_TTL_SECONDS as u32) * 1000;
const META_STORE_MIN_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const CAS_MAX_RETRIES: u32 = 3;
const DEFAULT_TIMEOUT_MS: i64 = 5_000;
const CDGC_REFRESH_BUDGET_MS: i64 = 15_000;
const DEFAULT_REFRESH_INTERVAL_SECONDS: i64 = 86_400;
const SEARCH_PATH: &str = "/ccgf-searchv2/api/v1/search";
const CT_FLATFIELD: &str = "com.infa.odin.models.file.flat.FlatField";
const REL_TECH_GLOSSARY: &str = "com.infa.ccgf.models.governance.IClassTechnicalGlossaryBase";
const ATTR_ISCDE: &str = "com.infa.ccgf.models.governance.isCDE";
const ATTR_SECURITY_CLASS: &str = "com.infa.ccgf.models.governance.securityClassification";
const DEFAULT_SENSITIVE_MARKER: &str = "confidential";
const DEFAULT_SENSITIVE_LEVELS: &str = "confidential,restricted";

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

#[derive(Clone)]
struct Ctx {
    fields: BTreeMap<String, String>,
}

/// JSON-RPC methods carrying data-product content worth enriching.
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
fn s(map: &Value, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_string)
}

async fn cdgc_auth(client: &HttpClient, config: &Config, clock: &Clock, start: SystemTime) -> Result<(String, String)> {
    let per_call = config.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
    let login_body = serde_json::to_vec(&json!({
        "username": config.cdgc_org_username, "password": config.cdgc_org_password,
    }))?;
    let t = next_call_timeout(per_call, elapsed_ms(start, clock.now())).ok_or_else(|| anyhow!("budget before Login"))?;
    let login_resp = client.request(&config.cdgc_login_url).path("/identity-service/api/v1/Login")
        .headers(vec![("Content-Type", "application/json")]).body(&login_body).timeout(t).post().await
        .map_err(|e| anyhow!("CDGC login failed: {e}"))?;
    if login_resp.status_code() >= 300 {
        return Err(anyhow!("CDGC login status {}", login_resp.status_code()));
    }
    let login: CdgcLoginResponse = serde_json::from_slice(login_resp.body()).map_err(|e| anyhow!("parse login: {e}"))?;
    let nonce = nonce_from_time(clock.now());
    let cookie = format!("USER_SESSION={}", login.session_id);
    let t = next_call_timeout(per_call, elapsed_ms(start, clock.now())).ok_or_else(|| anyhow!("budget before JWT"))?;
    let jwt_resp = client.request(&config.cdgc_login_url)
        .path(&format!("/identity-service/api/v1/jwt/Token?client_id=idmc_api&nonce={nonce}"))
        .headers(vec![("cookie", cookie.as_str()), ("IDS-SESSION-ID", login.session_id.as_str())])
        .timeout(t).get().await.map_err(|e| anyhow!("CDGC JWT failed: {e}"))?;
    if jwt_resp.status_code() >= 300 {
        return Err(anyhow!("CDGC JWT status {}", jwt_resp.status_code()));
    }
    let jwt: CdgcJwtResponse = serde_json::from_slice(jwt_resp.body()).map_err(|e| anyhow!("parse jwt: {e}"))?;
    Ok((jwt.jwt_token, login.org_id))
}

async fn cdgc_search(
    client: &HttpClient, config: &Config, clock: &Clock, start: SystemTime,
    jwt: &str, org: &str, body: &Value,
) -> Result<Vec<Value>> {
    let per_call = config.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
    let authz = format!("Bearer {jwt}");
    let payload = serde_json::to_vec(body)?;
    let t = next_call_timeout(per_call, elapsed_ms(start, clock.now())).ok_or_else(|| anyhow!("budget before search"))?;
    let resp = client.request(&config.cdgc_search_url).path(SEARCH_PATH)
        .headers(vec![
            ("Authorization", authz.as_str()),
            ("X-INFA-ORG-ID", org),
            ("X-INFA-SEARCH-LANGUAGE", "elasticsearch"),
            ("Content-Type", "application/json"),
        ])
        .body(&payload).timeout(t).post().await
        .map_err(|e| anyhow!("CDGC search failed: {e}"))?;
    if resp.status_code() >= 300 {
        return Err(anyhow!("CDGC search status {}", resp.status_code()));
    }
    let v: Value = serde_json::from_slice(resp.body()).map_err(|e| anyhow!("parse search: {e}"))?;
    Ok(v.get("hits").and_then(|h| h.get("hits")).and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|h| h.get("sourceAsMap").cloned()).collect())
        .unwrap_or_default())
}

/// Derive the governed metadata summary (header name → value) for a schema asset.
async fn fetch_summary(client: &HttpClient, config: &Config, clock: &Clock, schema_id: &str) -> Result<BTreeMap<String, String>> {
    let start = clock.now();
    let (jwt, org) = cdgc_auth(client, config, clock, start).await?;
    let sens_marker = config.sensitive_marker.as_deref().unwrap_or(DEFAULT_SENSITIVE_MARKER).to_lowercase();
    let sens_levels: Vec<String> = config.sensitive_levels.as_deref().unwrap_or(DEFAULT_SENSITIVE_LEVELS)
        .split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();

    let files = cdgc_search(client, config, clock, start, &jwt, &org, &json!({
        "from":0,"size":1,"query":{"bool":{"must":[
            {"terms":{"elementType":["OBJECT"]}},{"terms":{"core.identity":[schema_id]}}]}}
    })).await?;
    let file = files.into_iter().next().ok_or_else(|| anyhow!("schema asset '{schema_id}' not found"))?;
    let location = s(&file, "core.location").ok_or_else(|| anyhow!("schema asset has no core.location"))?;

    let cols = cdgc_search(client, config, clock, start, &jwt, &org, &json!({
        "from":0,"size":1000,"query":{"bool":{
            "must":[{"terms":{"core.classType":[CT_FLATFIELD]}}],
            "filter":[{"terms":{"core.location::path_hierarchy.parent":[location]}}]}}
    })).await?;
    let mut col_names: Vec<String> = Vec::new();
    let mut col_ids: Vec<String> = Vec::new();
    let mut id_to_name: BTreeMap<String, String> = BTreeMap::new();
    for c in &cols {
        if let (Some(id), Some(name)) = (s(c, "core.identity"), s(c, "core.name")) {
            col_names.push(name.clone());
            col_ids.push(id.clone());
            id_to_name.insert(id, name);
        }
    }

    // column → term links, then resolve terms for isCDE + sensitivity
    let mut required: Vec<String> = Vec::new();
    let mut sensitive: Vec<String> = Vec::new();
    if !col_ids.is_empty() {
        let rels = cdgc_search(client, config, clock, start, &jwt, &org, &json!({
            "from":0,"size":5000,"query":{"bool":{"must":[
                {"terms":{"elementType":["RELATIONSHIP"]}},
                {"terms":{"type":[REL_TECH_GLOSSARY]}},
                {"terms":{"core.sourceIdentity":col_ids}}]}}
        })).await?;
        let mut term_to_cols: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in &rels {
            if let (Some(src), Some(tgt)) = (s(r, "core.sourceIdentity"), s(r, "core.targetIdentity")) {
                if let Some(name) = id_to_name.get(&src) {
                    term_to_cols.entry(tgt).or_default().push(name.clone());
                }
            }
        }
        let term_ids: Vec<String> = term_to_cols.keys().cloned().collect();
        if !term_ids.is_empty() {
            let terms = cdgc_search(client, config, clock, start, &jwt, &org, &json!({
                "from":0,"size":5000,"query":{"bool":{"must":[
                    {"terms":{"elementType":["OBJECT"]}},{"terms":{"core.identity":term_ids}}]}}
            })).await?;
            for t in &terms {
                let Some(tid) = s(t, "core.identity") else { continue };
                let cols_for = term_to_cols.get(&tid).cloned().unwrap_or_default();
                if t.get(ATTR_ISCDE).and_then(Value::as_bool).unwrap_or(false) {
                    required.extend(cols_for.clone());
                }
                // Primary: the term's structured Security Level classification.
                // Fallback (only when no level is set): the description substring marker.
                let level = s(t, ATTR_SECURITY_CLASS).unwrap_or_default().trim().to_lowercase();
                let is_sensitive = if level.is_empty() {
                    let desc = s(t, "core.description").unwrap_or_default().to_lowercase();
                    desc.contains(&sens_marker)
                } else {
                    sens_levels.contains(&level)
                };
                if is_sensitive {
                    sensitive.extend(cols_for);
                }
            }
        }
    }
    required.sort();
    required.dedup();
    sensitive.sort();
    sensitive.dedup();

    let mut fields = BTreeMap::new();
    if let Some(n) = s(&file, "core.name") {
        fields.insert("x-dp-name".to_string(), n);
    }
    if let Some(e) = s(&file, "core.externalId") {
        fields.insert("x-dp-external-id".to_string(), e);
    }
    // Source = the asset's catalog/scan origin (CDGC core.origin), derived from the asset itself.
    if let Some(origin) = s(&file, "core.origin") {
        fields.insert("x-dp-source".to_string(), origin);
    }
    fields.insert("x-dp-field-count".to_string(), col_names.len().to_string());
    if !col_names.is_empty() {
        fields.insert("x-dp-fields".to_string(), col_names.join(","));
    }
    if !required.is_empty() {
        fields.insert("x-dp-required".to_string(), required.join(","));
    }
    if !sensitive.is_empty() {
        fields.insert("x-dp-sensitive".to_string(), sensitive.join(","));
    }
    Ok(fields)
}

async fn read_cached<S: DataStorage>(store: &S, key: &str) -> Option<CachedMeta> {
    match store.get::<CachedMeta>(key).await {
        Ok(Some((c, _))) => Some(c),
        Ok(None) => None,
        Err(e) => {
            logger::warn!("cmi: cache read failed: {e}");
            None
        }
    }
}
async fn write_cached<S: DataStorage>(store: &S, key: &str, entry: &CachedMeta) {
    for _ in 0..CAS_MAX_RETRIES {
        match store.get::<CachedMeta>(key).await {
            Ok(Some((_, v))) => match store.store(key, &StoreMode::Cas(v), entry).await {
                Ok(()) => return,
                Err(DataStorageError::CasMismatch) => continue,
                Err(e) => { logger::warn!("cmi: persist failed: {e}"); return; }
            },
            Ok(None) => match store.store(key, &StoreMode::Absent, entry).await {
                Ok(()) => return,
                Err(DataStorageError::CasMismatch) => continue,
                Err(e) => { logger::warn!("cmi: persist failed: {e}"); return; }
            },
            Err(e) => { logger::warn!("cmi: read-before-persist failed: {e}"); return; }
        }
    }
}
async fn try_acquire_refresh_lock<S: DataStorage>(store: &S, key: &str, now: i64) -> Result<bool, DataStorageError> {
    let entry = RefreshLock { acquired_at: now };
    match store.store(key, &StoreMode::Absent, &entry).await {
        Ok(()) => Ok(true),
        Err(DataStorageError::CasMismatch) => match store.get::<RefreshLock>(key).await? {
            Some((existing, v)) => {
                if now - existing.acquired_at < REFRESH_LOCK_TTL_SECONDS { Ok(false) }
                else {
                    match store.store(key, &StoreMode::Cas(v), &entry).await {
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

async fn get_summary<S: DataStorage>(
    client: &HttpClient, config: &Config, clock: &Clock, meta_store: &S, lock_store: &S, schema_id: &str,
) -> Option<BTreeMap<String, String>> {
    let key = format!("{META_CACHE_KEY_PREFIX}{schema_id}");
    let ttl = config.refresh_interval_seconds.unwrap_or(DEFAULT_REFRESH_INTERVAL_SECONDS).max(0);
    let now = now_secs(clock);
    let cached = read_cached(meta_store, &key).await;
    if let Some(c) = &cached {
        if now - c.timestamp < ttl {
            return Some(c.fields.clone());
        }
    }
    let lock_key = format!("{REFRESH_LOCK_KEY_PREFIX}{schema_id}");
    if !try_acquire_refresh_lock(lock_store, &lock_key, now).await.unwrap_or(true) {
        return cached.map(|c| c.fields);
    }
    match fetch_summary(client, config, clock, schema_id).await {
        Ok(fields) => {
            write_cached(meta_store, &key, &CachedMeta { fields: fields.clone(), timestamp: now }).await;
            Some(fields)
        }
        Err(e) => {
            logger::warn!("cmi: metadata refresh failed for '{schema_id}': {e}");
            if config.fail_open_on_cdgc_error.unwrap_or(true) { cached.map(|c| c.fields) } else { None }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn request_filter<S: DataStorage>(
    request_state: RequestState,
    config: Rc<Config>,
    client: Rc<HttpClient>,
    clock: Rc<Clock>,
    meta_store: Rc<S>,
    lock_store: Rc<S>,
) -> Flow<Option<Ctx>> {
    let hs = request_state.into_headers_state().await;
    // Optionally bind the schema id to a validated JWT claim (decoded here, verified
    // by an upstream JWT Validation policy). A configured claim wins over the header;
    // absent config or absent claim falls back to the header (backward compatible).
    let claim_schema = config.schema_id_claim.as_deref().and_then(|name| {
        let auth = hs.handler().header("authorization");
        claims::decode_bearer_claims(auth.as_deref())
            .and_then(|c| claims::claim_str(&c, name))
    });
    let header_name = config.schema_id_header.as_deref().unwrap_or("x-dp-schema-id");
    let schema_id = claim_schema
        .or_else(|| hs.handler().header(header_name).filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| config.schema_id.clone());

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
    let fields = get_summary(&client, &config, &clock, &*meta_store, &*lock_store, &schema_id)
        .await
        .unwrap_or_default();
    Flow::Continue(Some(Ctx { fields }))
}

async fn response_filter(response_state: ResponseState, request_data: RequestData<Option<Ctx>>, config: Rc<Config>) {
    let ctx = match request_data {
        RequestData::Continue(Some(c)) => c,
        _ => return,
    };
    let hs = response_state.into_headers_state().await;
    let h = hs.handler();
    if !ctx.fields.is_empty() {
        for (name, value) in &ctx.fields {
            h.set_header(name, value);
        }
        h.set_header("x-dp-metadata-source", "cdgc");
        h.set_header("x-dp-metadata-status", "ok");
    } else if config.header_on_miss.unwrap_or(true) {
        h.set_header("x-dp-metadata-status", "unavailable");
    }
}

fn launch_policy<S: DataStorage + 'static>(
    launcher: Launcher, config: Rc<Config>, client: Rc<HttpClient>, clock: Rc<Clock>,
    meta_store: Rc<S>, lock_store: Rc<S>,
) -> impl std::future::Future<Output = Result<()>> {
    let cfg_req = config.clone();
    let filter = on_request(move |rs| {
        let c = cfg_req.clone();
        let cl = client.clone();
        let ck = clock.clone();
        let ms = meta_store.clone();
        let ls = lock_store.clone();
        async move { request_filter(rs, c, cl, ck, ms, ls).await }
    })
    .on_response(move |rs, rd| {
        let c = config.clone();
        async move { response_filter(rs, rd, c).await }
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
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|err| anyhow!("Failed to parse configuration '{}'. Cause: {}", String::from_utf8_lossy(&bytes), err))?;
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
