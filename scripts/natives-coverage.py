#!/usr/bin/env python3
"""Which Sui native functions move2sp1 implements.

Only system packages may declare natives (Sui's verifier rejects them elsewhere), so the
complete list is the natives of 0x1 (std), 0x2 (sui), 0x3 (sui_system) and 0xb (bridge) at
the current mainnet framework version, read from their disassembly via GraphQL. A native is
"implemented" if the compiler's native lowering (compiler/src/translate/{natives,sui}.rs)
handles `module::function`.

usage: scripts/natives-coverage.py [--markdown]
"""

import json
import re
import sys
import urllib.request
from pathlib import Path

URL = "https://graphql.mainnet.sui.io/graphql"
PACKAGES = ["0x1", "0x2", "0x3", "0xb"]
ROOT = Path(__file__).resolve().parent.parent


def gql(query, variables=None):
    req = urllib.request.Request(
        URL,
        data=json.dumps({"query": query, "variables": variables or {}}).encode(),
        headers={"content-type": "application/json", "user-agent": "sui-replay"},
    )
    d = json.load(urllib.request.urlopen(req, timeout=120))
    if "errors" in d:
        raise SystemExit(d["errors"])
    return d["data"]


def natives(pkg):
    out, after = [], None
    while True:
        d = gql(
            """query($a: SuiAddress!, $after: String) { package(address: $a) { version
                modules(first: 20, after: $after) { pageInfo { hasNextPage endCursor }
                  nodes { name disassembly } } } }""",
            {"a": pkg, "after": after},
        )["package"]
        for m in d["modules"]["nodes"]:
            for f in re.findall(r"^\s*native\s+(?:(?:public\(\w+\)|public|entry|friend|fun)\s+)*(\w+)\s*[<(]", m["disassembly"], re.M):
                out.append(f"{m['name']}::{f}")
        if not d["modules"]["pageInfo"]["hasNextPage"]:
            return d["version"], out
        after = d["modules"]["pageInfo"]["endCursor"]


def main():
    src = "".join(
        (ROOT / "compiler/src/translate" / f).read_text() for f in ["natives.rs", "sui.rs"]
    )
    rows = []
    for pkg in PACKAGES:
        version, fns = natives(pkg)
        for f in sorted(set(fns)):
            rows.append((pkg, version, f, f'"{f}"' in src))
    done = sum(r[3] for r in rows)
    if "--markdown" in sys.argv:
        print(f"| package | native | implemented |\n|---|---|---|")
        for pkg, v, f, ok in rows:
            print(f"| {pkg} (v{v}) | `{f}` | {'yes' if ok else 'no'} |")
    else:
        for pkg, v, f, ok in rows:
            print(f"{'ok  ' if ok else '----'} {pkg}::{f}")
    print(f"\n{done} of {len(rows)} natives implemented", file=sys.stderr)


if __name__ == "__main__":
    main()
