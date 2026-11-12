#!/usr/bin/env python3
"""Replay a sample of recent Sui mainnet transactions with `sui-replay` and tally the results.

usage: scripts/replay-sample.py [N] [--digests FILE]

Picks N (default 50) programmable transactions from the most recent checkpoints (or the
digests listed in FILE), replays each, and prints one line per transaction plus a summary:
MATCH (byte-identical / same failure), DIFF (replay disagrees with chain), UNSUPPORTED (a
clean "not supported" stop), ERROR (anything else). Results go to out/replay/sample.json.
"""

import json
import subprocess
import sys
import urllib.request
from collections import Counter

URL = "https://graphql.mainnet.sui.io/graphql"
BIN = "target/release/sui-replay"


def gql(query, variables=None):
    req = urllib.request.Request(
        URL,
        data=json.dumps({"query": query, "variables": variables or {}}).encode(),
        headers={"content-type": "application/json", "user-agent": "sui-replay"},
    )
    d = json.load(urllib.request.urlopen(req, timeout=60))
    if "errors" in d:
        raise SystemExit(d["errors"])
    return d["data"]


def recent_digests(n):
    out, before = [], None
    while len(out) < n:
        d = gql(
            """query($b: String) { transactions(last: 50, before: $b) {
                pageInfo { startCursor }
                nodes { digest kind { __typename } } } }""",
            {"b": before},
        )["transactions"]
        before = d["pageInfo"]["startCursor"]
        out += [
            t["digest"]
            for t in reversed(d["nodes"])
            if t["kind"]["__typename"] == "ProgrammableTransaction"
        ]
    return out[:n]


def classify(output, code):
    last = output.strip().splitlines()[-1] if output.strip() else ""
    if code == 0 and "MATCH" in last:
        return "MATCH", last
    reason = next(
        (l for l in output.splitlines() if l.startswith("Error:")), last
    ).removeprefix("Error: ")
    detail = [l.strip() for l in output.splitlines() if l.startswith("    ")]
    if detail:
        reason += " | " + "; ".join(detail[:3])
    if "DIFF" in output:
        return "DIFF", reason
    if "not supported" in reason or "not available" in reason:
        return "UNSUPPORTED", reason
    return "ERROR", reason


def main():
    args = sys.argv[1:]
    if "--digests" in args:
        i = args.index("--digests")
        digests = open(args[i + 1]).read().split()
        del args[i : i + 2]
    else:
        digests = recent_digests(int(args[0]) if args else 50)
    results = []
    for d in digests:
        p = subprocess.run([BIN, d], capture_output=True, text=True, timeout=900)
        verdict, reason = classify(p.stdout + p.stderr, p.returncode)
        results.append({"digest": d, "verdict": verdict, "reason": reason})
        print(f"{verdict:12} {d}  {reason[:150] if verdict != 'MATCH' else ''}", flush=True)
    counts = Counter(r["verdict"] for r in results)
    print("\n" + ", ".join(f"{k} {v}" for k, v in counts.most_common()), f"(of {len(results)})")
    json.dump(results, open("out/replay/sample.json", "w"), indent=1)


if __name__ == "__main__":
    main()
