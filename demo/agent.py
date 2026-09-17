#!/usr/bin/env python3
"""
Agent simulation for the catalog-driven Contract Metadata Injection demo.

The policy is configured with only a CDGC schema-asset id. It
derives the data product's governed identity from Informatica CDGC (the scanned
dim_product.csv: name, columns, and the required/sensitive flags from the columns'
linked Business Terms) and stamps it onto the response as x-dp-* headers.

Usage:
    CMI_GW_URL="https://<host>/catalog-meta-demo/mcp" python3 agent.py
"""
import json, os, ssl, sys, urllib.request

GW = (sys.argv[1] if len(sys.argv) > 1 else os.environ.get("CMI_GW_URL", "")).strip()
if not GW:
    sys.exit("Set CMI_GW_URL (governed endpoint). See demo/env.local.sh.example")
_CTX = ssl.create_default_context(); _CTX.check_hostname = False; _CTX.verify_mode = ssl.CERT_NONE


def call():
    body = {"jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "get_products", "arguments": {"variant": "leak"}}}
    req = urllib.request.Request(GW, data=json.dumps(body).encode(), method="POST", headers={
        "Content-Type": "application/json", "Accept": "application/json, text/event-stream",
        "Accept-Encoding": "identity", "mcp-session-id": "catalog-meta-demo"})
    try:
        resp = urllib.request.urlopen(req, timeout=25, context=_CTX)
        return resp.status, {k: v for k, v in resp.headers.items() if k.lower().startswith("x-dp-")}
    except urllib.error.HTTPError as e:
        return e.code, {k: v for k, v in e.headers.items() if k.lower().startswith("x-dp-")}


def main():
    print(f"🏷️  catalog-driven metadata injection  →  {GW}\n")
    st, dp = call()
    print(f"  get_products → HTTP {st}\n")
    print("  Governed identity headers derived live from CDGC (dim_product.csv):")
    for k in sorted(dp):
        print(f"    {k}: {dp[k]}")
    status = (dp.get("x-dp-metadata-status") or dp.get("X-Dp-Metadata-Status") or "").lower()
    print()
    if status == "ok":
        print("✅ The response is self-describing: name, column set, required (isCDE) and")
        print("   sensitive (term marked Confidential) fields — all from Informatica CDGC,")
        print("   from a config of just a schema-asset id.")
    else:
        print("⚠️  Metadata unavailable — check CDGC creds/ids/egress (fail-open: data still flowed).")


if __name__ == "__main__":
    main()
