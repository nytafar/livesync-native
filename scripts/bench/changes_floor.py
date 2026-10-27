#!/usr/bin/env python3
"""How long does the server take just to page through the filtered _changes feed?

    source scripts/bench/lib.sh
    python3 scripts/bench/changes_floor.py bench-lsync 250 1000 5000

Pages through `<db>/_changes` from 0 with the same request lsync makes
(filter=_selector, every document except chunks, include_docs, conflicts) at
each page size, and prints wall time, pages, bytes and documents. Read-only.
This is the floor for any client's cold sync that follows the feed this way.
"""
import base64
import json
import os
import sys
import time
import urllib.request

URL = os.environ["LSYNC_URL"].rstrip("/")
assert URL.startswith(("http://127.0.0.1:8411", "http://localhost:8411"))
AUTH = "Basic " + base64.b64encode(f"{os.environ['LSYNC_USER']}:{os.environ['LSYNC_PASSWORD']}".encode()).decode()
SELECTOR = json.dumps({"selector": {"$or": [{"type": {"$ne": "leaf"}}, {"_deleted": True}]}}).encode()


def scan(db, limit, include_docs=True):
    since, pages, nbytes, docs = "0", 0, 0, 0
    t0 = time.monotonic()
    while True:
        q = f"filter=_selector&limit={limit}&since={urllib.parse.quote(since)}&conflicts=true"
        if include_docs:
            q += "&include_docs=true"
        req = urllib.request.Request(f"{URL}/{db}/_changes?{q}", data=SELECTOR, method="POST",
                                     headers={"Authorization": AUTH, "Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=600) as r:
            body = r.read()
        nbytes += len(body)
        d = json.loads(body)
        pages += 1
        docs += len(d["results"])
        since = d["last_seq"]
        if d.get("pending", 0) == 0 or not d["results"]:
            break
    return time.monotonic() - t0, pages, nbytes, docs


if __name__ == "__main__":
    import urllib.parse  # noqa: F401
    db = sys.argv[1]
    for lim in sys.argv[2:]:
        wall, pages, nbytes, docs = scan(db, int(lim))
        print(json.dumps({"limit": int(lim), "wall_s": round(wall, 2), "pages": pages,
                          "mb": round(nbytes / 1e6, 1), "docs": docs}), flush=True)
