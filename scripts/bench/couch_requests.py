#!/usr/bin/env python3
"""Break a client run down by HTTP request, from the dev CouchDB's own request log.

    python3 scripts/bench/couch_requests.py DB SINCE UNTIL
    (SINCE/UNTIL as docker logs accepts them, e.g. 2026-10-27T08:00:00Z)

CouchDB logs one [notice] line per request with the response time in ms as
the last field, stamped when the response is done. Per endpoint this prints
the number of requests, their summed server time, the median, and the wall
time during which at least one request of that kind was in flight. "all"
is the union over every request: wall time the client spent waiting on the
server at all. Read-only; nothing in the client is changed.
"""
import collections
import datetime as dt
import re
import statistics
import subprocess
import sys

db, since, until = sys.argv[1], sys.argv[2], sys.argv[3]
out = subprocess.run(["docker", "logs", "--since", since, "--until", until, "lsync-dev-couchdb"],
                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True).stdout
line_re = re.compile(r"^\[notice\] (\S+) \S+ \S+ \S+ \S+ \S+ \S+ (\w+) (\S+) (\d+) \w+ (\d+)$")
reqs = collections.defaultdict(list)
for line in out.splitlines():
    m = line_re.match(line.strip())
    if not m:
        continue
    ts, method, path, _status, ms = m.groups()
    p = path.split("?")[0]
    if not (p == f"/{db}" or p.startswith(f"/{db}/")):
        continue
    rest = p[len(db) + 1:].lstrip("/")
    kind = rest.split("/")[0] if rest.startswith("_") else ("doc" if rest else "db-info")
    end = dt.datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
    reqs[f"{method} {kind}"].append((end - int(ms) / 1000, end, int(ms)))


def union(iv):
    total, cur_s, cur_e = 0.0, None, None
    for s, e, _ in sorted(iv):
        if cur_e is None or s > cur_e:
            if cur_e is not None:
                total += cur_e - cur_s
            cur_s, cur_e = s, e
        else:
            cur_e = max(cur_e, e)
    if cur_e is not None:
        total += cur_e - cur_s
    return total


allreq = [r for v in reqs.values() for r in v]
if not allreq:
    sys.exit("no requests found")
span = max(e for _, e, _ in allreq) - min(s for s, _, _ in allreq)
print(f"{db}: {len(allreq)} requests over {span:.1f} s; server busy (any request in flight) {union(allreq):.1f} s")
print(f"{'endpoint':<22}{'count':>8}{'sum s':>10}{'median ms':>11}{'in flight s':>13}")
for k, v in sorted(reqs.items(), key=lambda kv: -sum(x[2] for x in kv[1])):
    print(f"{k:<22}{len(v):>8}{sum(x[2] for x in v) / 1000:>10.1f}{statistics.median(x[2] for x in v):>11.0f}"
          f"{union(v):>13.1f}")
