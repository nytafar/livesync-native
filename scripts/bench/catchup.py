#!/usr/bin/env python3
"""Untimed catch-up for the official CLI: start its daemon on an existing
repetition's state and wait (without bench.py's timeout) until a given set of
writer files is in its vault.

    source scripts/bench/lib.sh
    BENCH_WORK=~/scratch/lsync-bench/run1 python3 scripts/bench/catchup.py official 1 inc10-r1 10

Used when the daemon's start-up scan outlasts bench.py's --timeout. Prints one
JSON line (also appended to results.jsonl as test "inc-<n>-untimed").
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bench  # noqa: E402

name, rep, tag, n = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4])
assert name == "official", "only the official CLI keeps its state between processes this way"
c = bench.Official.__new__(bench.Official)
c.root = os.path.join(bench.WORK, "official", f"r{rep}")
c.vault, c.dbdir = os.path.join(c.root, "vault"), os.path.join(c.root, "db")
c.settings = os.path.join(c.root, "cli.json")
c.proc = c.pid = None
want = {os.path.join(c.vault, f"bench-writes/{tag}/f{i:04d}.md"): bench.note_body(tag, i) for i in range(n)}
cnt = bench.Counters()
t0 = time.monotonic()
pid = c.start()
s = bench.Sampler(pid, every=2)
s.start()
t1 = bench.wait_files(want, timeout=4 * 3600, poll=0.5)
bench.record({"contender": name, "rep": rep, "db": c.db, "test": f"inc-{n}-untimed",
              "wall_s": round(t1 - t0, 2) if t1 else None, **s.done(), **cnt.done()})
c.stop()
os.chmod(c.settings, 0o600)
