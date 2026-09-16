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
