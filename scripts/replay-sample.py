#!/usr/bin/env python3
"""Replay a sample of recent Sui mainnet transactions with `sui-replay` and tally the results.

usage: scripts/replay-sample.py [N] [--digests FILE] [--jobs J]

Picks N (default 50) programmable transactions from the most recent checkpoints (or the
digests listed in FILE), replays each (J at a time, default 6: mostly network-bound), and prints one line per transaction plus a summary:
MATCH (byte-identical / same failure), DIFF (replay disagrees with chain), UNSUPPORTED (a
clean "not supported" stop), ERROR (anything else). Results go to out/replay/sample.json.
"""

import json
import subprocess
from concurrent.futures import ThreadPoolExecutor
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
    verdict = next((l for l in output.splitlines() if l.startswith("MATCH:")), None)
    if code == 0 and verdict:
        return "MATCH", verdict
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
    jobs = 6
    if "--jobs" in args:
        i = args.index("--jobs")
        jobs = int(args[i + 1])
        del args[i : i + 2]
    if "--digests" in args:
        i = args.index("--digests")
        digests = open(args[i + 1]).read().split()
        del args[i : i + 2]
    else:
        digests = recent_digests(int(args[0]) if args else 50)
    def replay(d):
        try:
            p = subprocess.run([BIN, d], capture_output=True, text=True, timeout=900)
            verdict, reason = classify(p.stdout + p.stderr, p.returncode)
        except subprocess.TimeoutExpired:
            verdict, reason = "ERROR", "timeout"
        print(f"{verdict:12} {d}  {reason[:150] if verdict != 'MATCH' else ''}", flush=True)
        return {"digest": d, "verdict": verdict, "reason": reason}

    with ThreadPoolExecutor(jobs) as pool:
        results = list(pool.map(replay, digests))
    counts = Counter(r["verdict"] for r in results)
    print("\n" + ", ".join(f"{k} {v}" for k, v in counts.most_common()), f"(of {len(results)})")
    json.dump(results, open("out/replay/sample.json", "w"), indent=1)


if __name__ == "__main__":
    main()
