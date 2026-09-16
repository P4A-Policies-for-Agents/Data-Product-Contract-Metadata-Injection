# Roadmap

## 1. Shared CDGC client for the data-product suite (the point of this policy)

This policy is the **front door**: it authenticates to IDMC once (Login → JWT →
data360), caches, and republishes contract metadata as `x-dp-*` headers. The
other four data-product policies (Usage-Terms, Subscription, Field-Level
Entitlement, Lineage) should **read those headers** rather than each opening its
own IDMC connection. Extract the CDGC client (auth chain + cache-aside + refresh
lock + `format: service` wiring) into a shared module the whole suite cites, so
IDMC is authenticated to in exactly one place.

## 2. JWT / session caching

Today a stale-cache request re-runs Login+JWT+Detail (3 calls). Fine at a 24 h
metadata TTL. If the TTL shortens or one instance fans across **many** assets,
cache the JWT in DataStorage keyed by org (respecting its `exp`) to collapse 3
calls → 1 — treating the stored JWT as a secret (short TTL, never logged, same
status-code-only discipline).

## 3. Dynamic multi-asset resolution

`assetIdHeader` already lets a request name its asset. Extend to derive the CDGC
asset id from the route / a contract registry, and cache per-asset, so one policy
instance self-scopes across a whole data mesh.

## 4. Richer, verified field map

Ship per-tenant `fieldMap` presets once real CDGC asset-detail shapes are
confirmed (owner/stakeholders, version, SLA/refresh, permitted purposes, glossary
term bindings), and optionally an embedded JSON `x-dp-contract` block in addition
to discrete headers.

## 5. OAuth client-credentials

If the org exposes OAuth client-credentials for IDMC, prefer it over storing a
service-account password; keep the least-privilege read-only account + Flex Secret
otherwise.

## 6. Emit an audit/decision record

Optionally log a structured record of each enrichment (asset, fields stamped,
cache hit/miss, source) for the observability suite.

## Known limitation — cold cache on streaming responses

This policy stamps response **headers** after resolving CDGC metadata. On a cold
cache it fetches CDGC on the **response leg**, which can race the streamed
(SSE) response-head commit and return **500** on a freshly-deployed instance
(a warm instance serves from cache and is fine). Attempts to move the fetch to
the request leg (Exchange versions 1.0.2–1.0.4, incl. `enable_stop_iteration`)
did not resolve it on the tested Omni runtime and are **superseded** — this repo
tracks the **1.0.1** code, which works on warm / non-streaming paths.

**Robust alternative:** the **Data Product Contract Conformance Guard** buffers the
response body while it fetches CDGC (so its fetch is race-free) and, from that same
fetch, emits the contract **identity** (name/externalId) *and* enforces the field
contract — delivering the self-describe + enforce story in one policy on a fresh
instance. Prefer it when you need identity on a cold/streaming path; use this
header-only policy for enrichment on warm/non-streaming paths.
