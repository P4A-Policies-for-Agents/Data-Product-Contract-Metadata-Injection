# Data Product Contract Metadata Injection — MuleSoft Omni/Flex Gateway Policy

An **inbound, response-header-only, fail-open** custom policy for the MuleSoft
Omni/Flex Gateway that makes a data product's **output port self-describing**: it
fetches a **CDGC-governed asset's metadata from Informatica IDMC** and stamps it
onto every response as `x-dp-*` headers (name, version, owner, classification,
SLA, purposes, live DQ score, …). It is the **keystone** of the data-product
policy suite — once contract context travels as cheap headers, every downstream
governance policy (purpose binding, entitlement, freshness, residency, usage
terms) becomes a header read instead of its own catalog lookup.

Built with the PDK, Rust → `wasm32-wasip1`, split-model. Works across **MCP**,
**A2A**, and **REST/HTTP**.

> **Enrichment, not enforcement.** This policy adds context and **fails open** —
> a CDGC outage degrades to serving last-known-good (or an
> `x-dp-metadata-status: unavailable` marker); it never blocks or errors the
> response. It says nothing about *who* the caller is — put auth ahead of it.

---

## How it connects to IDMC (the important part)

The connectivity + auth pattern is lifted from the accepted **IDMC Data Quality
Gate** (`bmantha/infa-omni-pdk`) — the sanctioned way a PDK/WASM policy reaches
an external service:

1. **Connectivity via `format: service`.** `cdgcLoginUrl` and `cdgcBaseApiUrl`
   are declared `format: service` in `gcl.yaml`; the generated `init()` registers
   them as **outbound clusters** (`abi.service_create(...)`), and the injected
   **`HttpClient`** dispatches to them. (A WASM policy cannot open an arbitrary
   socket — `format: service` is how you grant it egress to IDMC.)
2. **Authentication — the CDGC chain**, run only on a cache miss/stale entry,
   bounded by a per-call `timeout` and an overall ~10 s budget:

   ```
   1. Login   POST {cdgcLoginUrl}/identity-service/api/v1/Login   {username,password} → {sessionId, orgId}
   2. JWT     GET  {cdgcLoginUrl}/identity-service/api/v1/jwt/Token?client_id=idmc_api&nonce=<nonce>
              header IDS-SESSION-ID: <sessionId>                    → {jwt_token}
   3. Detail  GET  {cdgcBaseApiUrl}/data360/search/v1/assets/{assetId}?scheme=internal&segments=<segments>
              headers Authorization: Bearer <jwt>, X-INFA-ORG-ID: <orgId>   → asset-detail JSON
   ```
3. **Extract → cache → stamp.** The configured `fieldMap` (header → path) is
   resolved against the asset-detail JSON; the extracted `header→value` map is
   cached in PDK **DataStorage** for `refreshIntervalSeconds` (default 24 h,
   single-flight refresh lock, `distributed` for cross-replica). On the response
   leg the fields are set as headers.

**Credentials** are `security:sensitive` (supply via a Flex Secret, never
plaintext). **Secret hygiene:** the policy logs CDGC **status codes only, never
response bodies** (Login/JWT bodies carry the session id / JWT).

---

## Field mapping (tune to your tenant)

CDGC's asset-detail JSON uses **dotted literal keys** (e.g. `core.score`), so the
`fieldMap` path language splits on **`/`** — a segment may itself contain dots,
and a numeric segment indexes an array:

```json
{
  "x-dp-name":           "core.name",
  "x-dp-classification": "core.classification",
  "x-dp-dq-score":       "dataQuality/0/core.score"
}
```

`segments` controls which detail segments are requested (`core,dataQuality`,
optionally `stakeholders,customAttributes`, …). **Verify the exact paths against
your tenant's response shape** once connected — the defaults are a starting point,
not a guarantee. Any path that doesn't resolve is simply skipped.

---

## Configuration reference

| Property | Type | Default | Description |
|---|---|---|---|
| `cdgcLoginUrl` | string (service) | — required | IDMC login base URL (POD-specific `dm-`/`dmp-`). |
| `cdgcBaseApiUrl` | string (service) | — required | CDGC base API URL (configure explicitly). |
| `cdgcOrgUsername` | string (sensitive) | — required | IDMC read-only service account. |
| `cdgcOrgPassword` | string (sensitive) | — required | Service-account password (Flex Secret). |
| `cdgcAssetId` | string | — required | CDGC asset id to resolve (default when `assetIdHeader` absent). |
| `assetIdHeader` | string | `x-dp-contract-id` | Request header overriding the asset id per request (multi-product). |
| `segments` | string | `core,dataQuality` | CDGC detail segments to request. |
| `fieldMap` | string (JSON) | name/classification/dq-score | header → `/`-path into the asset detail. |
| `headerOnMiss` | boolean | `true` | Stamp `x-dp-metadata-status: unavailable` when metadata can't be resolved. |
| `refreshIntervalSeconds` | integer (≥30) | `86400` | Cache TTL before a lazy CDGC refresh. |
| `failOpenOnCdgcError` | boolean | `true` | Serve last-known-good on a transient CDGC error. |
| `distributed` | boolean | `false` | Share cache + refresh lock across replicas (needs shared storage). |
| `timeout` | integer (ms) | `5000` | Per-call CDGC timeout (clamped to the ~10 s chain budget). |

Always-stamped: `x-dp-asset-id`, `x-dp-metadata-source: cdgc`, `x-dp-metadata-status: ok|unavailable`.

---

## Repository layout

```
contract-metadata-injection-definition/   # gcl.yaml (schema), exchange.json, Makefile
contract-metadata-injection-flex/          # Rust implementation
  src/lib.rs        # CDGC fetch chain + cache-aside (CAS + refresh lock) + request/response filters
  src/cdgc.rs       # PURE: path resolver, field extraction, cached types — 8 unit tests
  src/generated/    # config.rs (Service types + service_create) generated from gcl.yaml
demo/
  mcp-metadata.json     # Sales Orders data-product MCP manifest (published type=mcp)
  config.json.example   # policy config template (fill in your CDGC creds/url/assetId)
  agent.py              # agent that prints the x-dp-* headers the gateway stamped — LIVE
  demo.sh · env.local.sh.example · PROVISION.md
```

---

## Build, test & release

```bash
cd contract-metadata-injection-definition && make release
cd ../contract-metadata-injection-flex
make build-asset-files
cargo build --target wasm32-wasip1 --release
cargo test --lib            # 8 pure unit tests (path resolver + field extraction)
make release
```

Published at **1.0.0** (definition + implementation). Requires **PDK 1.10**
(`HttpClient`, `format: service`, `Clock`, DataStorage CAS).

---

## Live demo

A mock **Sales Orders** data product (`get_orders`) is fronted by the gateway with
this policy applied. The agent calls it and prints the `x-dp-*` headers the gateway
stamped from CDGC.

```bash
cp demo/config.json.example demo/config.json      # fill in your IDMC creds/url/assetId
# provision per demo/PROVISION.md, then:
cp demo/env.local.sh.example demo/env.local.sh     # set CMI_GW_URL
./demo/demo.sh
```

> The CDGC side of the demo needs a real IDMC tenant (service account + asset id).
> The gateway must also have the IDMC hosts reachable as the policy's `service`
> egress. See PROVISION for the wiring.

---

## Design notes & credits

- **Pure core** (`cdgc.rs`): `resolve_path` (`/`-split, dotted-literal-key aware,
  array indexing), `build_fields`, `percent_encode`, cached types. 8 unit tests.
- **Cache-aside** with `StoreMode::Absent`/`Cas` writes (never DELETE — gossip
  tombstone safety) and a single-flight refresh lock, so under load only one
  request pays the CDGC round trip.
- **Response-leg, header-only** enrichment (no body read) → SSE / token-streaming
  safe.
- **Auth + connectivity pattern credited to** `bmantha/infa-omni-pdk` (IDMC Data
  Quality Gate). This policy is designed to **share that CDGC client** — see
  [roadmap.md](roadmap.md): the suite should authenticate to IDMC in one place
  (this front-door policy) and have the other data-product policies read the
  `x-dp-*` headers it publishes.

---

## Skills used

- **PDK** (`omni-gateway-pdk-skills`): `pdk-create-policy`, `pdk-mcp`,
  `pdk-request-headers-bodies`, `pdk-data-storage`, `pdk-distributed-cache-gossip`,
  `pdk-schema-definition`, `pdk-unit-tests`.
- **P4A** (`p4a-skills`): `p4a-build-policy`, `p4a-verify-requirements`,
  `p4a-mcp-usage`, `p4a-test-mcp-policies-with-a2d`.
