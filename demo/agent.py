#!/usr/bin/env python3
"""
Agent simulation for the Data Product Contract Metadata Injection demo.

An agent calls the `get_orders` MCP tool of the Sales Orders data product through
the governed gateway. The gateway policy resolves the product's CDGC-governed
metadata from Informatica IDMC (cached) and stamps it onto the response as
x-dp-* headers, so the agent receives a self-describing output port: it can read
the contract name, classification, and live DQ score alongside the data.

Dependency-free (stdlib). Prints the x-dp-* response headers (the enrichment).

Usage:
    CMI_GW_URL="https://<host>/contract-meta-demo/mcp" python3 agent.py
"""
import json, os, ssl, sys, urllib.request

GW = (sys.argv[1] if len(sys.argv) > 1 else os.environ.get("CMI_GW_URL", "")).strip()
if not GW:
    sys.exit("Set CMI_GW_URL (or pass the gateway MCP URL as arg1). See demo/env.local.sh.example")
_CTX = ssl.create_default_context(); _CTX.check_hostname = False; _CTX.verify_mode = ssl.CERT_NONE


def call_get_orders():
    body = {"jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "get_orders", "arguments": {"limit": 2}}}
    req = urllib.request.Request(GW, data=json.dumps(body).encode(), method="POST", headers={
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        "Accept-Encoding": "identity",
        "mcp-session-id": "contract-meta-demo",
    })
    try:
        resp = urllib.request.urlopen(req, timeout=25, context=_CTX)
        status, hdrs, raw = resp.status, resp.headers, resp.read().decode()
    except urllib.error.HTTPError as e:
        status, hdrs, raw = e.code, e.headers, e.read().decode()
    dp = {k: v for k, v in hdrs.items() if k.lower().startswith("x-dp-")}
    for line in raw.splitlines():
        if line.startswith("data:"):
            raw = line[len("data:"):].strip(); break
    try:
        result = json.loads(raw).get("result", {})
        result = result.get("structuredContent") or result.get("content") or result
    except Exception:
        result = {"raw": raw}
    return status, dp, result


def main():
    print(f"🧾 data-product agent  →  {GW}\n")
    st, dp, result = call_get_orders()
    print(f"  get_orders → HTTP {st}\n")
    print("  Contract-metadata headers stamped by the gateway (from CDGC):")
    if dp:
        for k in sorted(dp):
            print(f"    {k}: {dp[k]}")
    else:
        print("    (none — is the policy applied? check x-dp-metadata-status)")
    print(f"\n  Data payload: {result}\n")
    status = dp.get("x-dp-metadata-status") or dp.get("X-Dp-Metadata-Status")
    if status == "ok":
        print("✅ The output port is self-describing: the agent received the contract's")
        print("   name / classification / DQ score as headers alongside the data — sourced")
        print("   live from Informatica CDGC and cached at the gateway.")
    elif status == "unavailable":
        print("⚠️  Metadata unavailable (CDGC creds/asset not resolvable yet, or cold cache).")
        print("   The policy fails open — the data still flowed. Check the CDGC config.")
    else:
        print("⚠️  No x-dp-* headers — confirm the policy is applied to this instance.")


if __name__ == "__main__":
    main()
