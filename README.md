# Data Product Contract Metadata Injection — MuleSoft Omni/Flex Gateway Policy

An **inbound, headers-only, fail-open** custom policy for the MuleSoft Omni/Flex
Gateway that makes a data product's output port **self-describing**: given only a
**CDGC asset id** for the scanned schema (a flat file, table, etc.), it derives
the product's governed identity from Informatica CDGC and stamps it onto every response as
`x-dp-*` headers. No per-field configuration — the field set, required flags and
sensitivity are read from the catalog at runtime.

Built with the PDK, Rust → `wasm32-wasip1`, split-model. Applies to **MCP** and
**REST/HTTP APIs** (`assetTypes: mcp,rest,http`) — both bind to a data-product
schema. (A2A was dropped: agents aren't bound to a schema.) This policy is
headers-only, so it is transport-agnostic across both.

> Enrichment only — it adds headers and **fails open** (a CDGC outage degrades to
> `x-dp-metadata-status: unavailable`; it never blocks the response). For
> enforcement (strip/reject drift), use the **Contract Conformance Guard**, which
> shares this catalog-driven CDGC client.

---

## How it works (catalog-driven)

On the **request leg** (await-safe under `enable_stop_iteration`), on a cache miss
it authenticates to IDMC (Login → JWT) and then, via the CDGC search API
**`POST cdgc-api…/ccgf-searchv2/api/v1/search`** (Elasticsearch DSL):

1. **Resolve the schema asset** (`core.identity = schemaId`) → name, external id, location.
2. **Enumerate its columns** (`FlatField` under the asset location) → field names + count.
3. **Column → Business Term links** + **resolve terms** → `required` (term `isCDE`) and `sensitive` (term description contains `sensitiveMarker`, e.g. *"Confidential…"*).

It caches the result (lazy refresh, single-flight, `distributed` opt-in) and stamps
the summary **synchronously on the response leg** — so the slow fetch never races
the streamed response-head commit.

Headers stamped (example, `dim_product.csv`):
```
x-dp-name: dim_product.csv          x-dp-field-count: 11
x-dp-external-id: …~FlatFile        x-dp-source: <scan/catalog origin id>
x-dp-fields: department,category,…,list_price,unit_cost,sku
x-dp-required: list_price,sku,unit_cost      x-dp-sensitive: unit_cost
x-dp-metadata-source: cdgc          x-dp-metadata-status: ok
```

---

## Configuration reference

| Property | Type | Default | Description |
|---|---|---|---|
| `cdgcLoginUrl` | string (service) | required | IDMC login base URL. |
| `cdgcSearchUrl` | string (service) | required | CDGC search host (`ccgf-searchv2`), e.g. `https://cdgc-api.<pod>.informaticacloud.com`. |
| `cdgcOrgUsername` / `cdgcOrgPassword` | string (sensitive) | required | IDMC read-only service account. |
| `schemaId` | string | required | CDGC asset id of the scanned schema (flat file, table, etc.) whose columns are summarized. |
| `schemaIdHeader` | string | `x-dp-schema-id` | Per-request schema-asset id override. |
| `sensitiveMarker` | string | `confidential` | Case-insensitive substring in a field's term description that marks it sensitive. |
| `headerOnMiss` | boolean | `true` | Stamp `x-dp-metadata-status: unavailable` when CDGC can't be resolved. |
| `refreshIntervalSeconds` | integer | `86400` | Metadata cache TTL. |
| `failOpenOnCdgcError` | boolean | `true` | Serve last-known-good on transient CDGC error. |
| `distributed` | boolean | `false` | Share cache + refresh lock across replicas. |
| `timeout` | integer (ms) | `5000` | Per-CDGC-call timeout (≤ ~15s chained budget). |

---

## Build, test & release

```bash
cd contract-metadata-injection-definition && make release
cd ../contract-metadata-injection-flex
make build-asset-files && cargo build --target wasm32-wasip1 --release
make release
```
Published at **1.0.6** (catalog-driven; drops `catalogId` and renames
`flatFileId`→`schemaId`, with `x-dp-source` now derived from the asset's
`core.origin`). Requires **PDK 1.10** with `enable_stop_iteration` (request-leg CDGC fetch).

> **Note on earlier versions:** 1.0.1 used a data360 detail-read + a configured
> field map and fetched on the *response* leg, which raced the streamed
> response-head commit and 500'd on a cold instance. 1.0.5 moved the fetch to the
> request leg (`enable_stop_iteration`) and derives the contract from the catalog —
> **verified robust on a freshly-deployed instance.**

---

## Live demo

```bash
cp demo/config.json.example demo/config.json     # fill CDGC creds/urls + schemaId
# provision per demo/PROVISION.md, then:
cp demo/env.local.sh.example demo/env.local.sh    # set CMI_GW_URL
./demo/demo.sh
```
The agent calls the product mock through the gateway and prints the `x-dp-*`
headers the gateway derived from CDGC (`dim_product.csv`).

---

## Skills used

- **PDK** (`omni-gateway-pdk-skills`): `pdk-create-policy`, `pdk-mcp`,
  `pdk-request-headers-bodies`, `pdk-data-storage`, `pdk-distributed-cache-gossip`,
  `pdk-schema-definition`.
- **P4A** (`p4a-skills`): `p4a-build-policy`, `p4a-verify-requirements`,
  `p4a-mcp-usage`, `p4a-test-mcp-policies-with-a2d`.
- **IDMC** (`governed-data-product-skills`, `IDMC - Data Governance Skills`):
  CDGC `ccgf-searchv2` search graph, MCC-scanned columns, Business Terms.
