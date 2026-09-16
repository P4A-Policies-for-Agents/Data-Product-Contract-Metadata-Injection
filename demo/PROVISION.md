# Demo provisioning runbook

Stands up the live Data Product Contract Metadata Injection demo with
`anypoint-cli-v4` (already authenticated) + the A2D MCP tools — **no bearer token
needed** for the gateway side. The **CDGC side needs a real IDMC tenant** (a
read-only service account + a governed asset id) supplied in `config.json`.
Replace `<...>` placeholders (identifiers, not secrets).

| Placeholder | What it is |
|---|---|
| `<orgId>` | Business-group / org id |
| `<mockServerId>` | A2D data-product mock MCP server id |
| `<gatewayId>` | Managed Flex Gateway resource id **with a public ingress** |
| `<gatewayPublicHost>` | Gateway public ingress URL |
| `<apiInstanceId>` | API Manager instance id |

Governed endpoint: `https://<gatewayPublicHost>/contract-meta-demo/mcp`

## 1. Publish the data-product mock to Exchange (type=mcp)

```bash
anypoint-cli-v4 exchange:asset:upload --name "Sales Orders Data Product" \
  --type mcp --status published --properties='{"platform":"a2d"}' \
  --files='{"mcp-metadata.json":"./mcp-metadata.json"}' sales-orders-data-product/1.0.0
```

## 2. Create + deploy the MCP Flex instance

```bash
anypoint-cli-v4 api-mgr:api:manage sales-orders-data-product 1.0.0 <orgId> \
  --environment Sandbox --isFlex --type mcp \
  --uri "https://www.a2d-ai.com/api/platform/<mockServerId>/" --apiInstanceLabel contract-meta-demo
anypoint-cli-v4 api-mgr:api:edit <apiInstanceId> --environment Sandbox --isFlex --type mcp \
  --withProxy --scheme http --port 8081 --path "/contract-meta-demo/" \
  --uri "https://www.a2d-ai.com/api/platform/<mockServerId>/"
anypoint-cli-v4 api-mgr:api:deploy <apiInstanceId> --environment Sandbox \
  --target <gatewayId> --gatewayVersion 1.0.0 --overwrite
```

## 3. Wire IDMC egress + apply the policy

The policy's `cdgcLoginUrl` / `cdgcBaseApiUrl` are `format: service` — the gateway
must be able to **reach those IDMC hosts as an outbound service**. On a managed
gateway confirm egress to `*.informaticacloud.com` is permitted; the policy's
`service_create` registers the cluster from the config URL.

```bash
cp config.json.example config.json    # fill cdgcLoginUrl/BaseApiUrl/Username/Password/AssetId
anypoint-cli-v4 api-mgr:policy:apply <apiInstanceId> contract-metadata-injection \
  --environment Sandbox --groupId <orgId> --policyVersion 1.0.0 --configFile ./config.json
anypoint-cli-v4 api-mgr:api:redeploy <apiInstanceId> --environment Sandbox
```

## 4. Run the agent

```bash
cp env.local.sh.example env.local.sh   # set CMI_GW_URL to the governed endpoint
./demo.sh
```

Expected: `get_orders` returns 200; the response carries `x-dp-metadata-status: ok`
plus the mapped `x-dp-*` headers (name, classification, DQ score) resolved from
CDGC. If CDGC isn't reachable/resolvable, the policy fails open and stamps
`x-dp-metadata-status: unavailable` (the data still flows).

## Notes

- **Two distinct upstreams:** the *route* target is the data-product mock; the
  *policy's* CDGC calls go to the IDMC hosts via the `format: service` egress —
  they are separate.
- **Field paths are tenant-specific.** After first connect, inspect the real CDGC
  asset-detail JSON and adjust `fieldMap`/`segments` in `config.json` (paths use
  `/` separators; CDGC keys like `core.score` are literal).
- `api:manage` alone leaves `deployment: null`; you need `api:edit --withProxy`
  then `api:deploy`. Use a gateway whose `configuration.ingress.publicUrl` is
  non-empty (else 404).
