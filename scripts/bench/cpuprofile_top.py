#!/usr/bin/env python3
"""Top self-time functions and modules in a V8 .cpuprofile (node --cpu-prof).

    python3 scripts/bench/cpuprofile_top.py FILE.cpuprofile [N]

Self time per sample is attributed to the sampled (leaf) node. "Module" is
the script URL shortened to its package or file, so PouchDB, leveldown, the
CLI bundle, and Node internals show up as separate rows; (garbage collector),
(program) and (idle) are V8's own buckets.
"""
import collections
import json
import re
import sys

prof = json.load(open(sys.argv[1]))
top_n = int(sys.argv[2]) if len(sys.argv) > 2 else 30
nodes = {n["id"]: n for n in prof["nodes"]}
deltas = prof.get("timeDeltas", [])
self_us = collections.Counter()
for sid, dt in zip(prof["samples"], deltas):
    self_us[sid] += dt
total = sum(self_us.values()) or 1


def module(url):
    if not url:
        return "(native/vm)"
    m = re.search(r"node_modules/((?:@[^/]+/)?[^/]+)", url)
    if m:
        return m.group(1)
    if url.startswith("node:"):
        return url
    return url.rsplit("/", 1)[-1]


by_fn, by_mod = collections.Counter(), collections.Counter()
for nid, us in self_us.items():
    cf = nodes[nid]["callFrame"]
    name = cf["functionName"] or "(anonymous)"
    by_fn[f"{name}  [{module(cf['url'])}:{cf['lineNumber'] + 1}]"] += us
    by_mod[module(cf["url"]) if not name.startswith("(") else name] += us
print(f"profile: {total / 1e6:.1f} s sampled")
print("\nby module / bucket (self time):")
for k, us in by_mod.most_common(15):
    print(f"  {100 * us / total:5.1f}%  {us / 1e6:7.1f} s  {k}")
print(f"\ntop {top_n} functions (self time):")
for k, us in by_fn.most_common(top_n):
    print(f"  {100 * us / total:5.1f}%  {us / 1e6:7.1f} s  {k}")

# Inclusive time: a function counts once per sample in which it is on the stack.
parent = {}
for n in prof["nodes"]:
    for c in n.get("children", []):
        parent[c] = n["id"]
incl = collections.Counter()
stack_cache = {}


def stack_keys(nid):
    if nid in stack_cache:
        return stack_cache[nid]
    keys, cur = set(), nid
    while cur is not None:
        cf = nodes[cur]["callFrame"]
        if cf["url"]:
            keys.add(f"{cf['functionName'] or '(anonymous)'}  [{module(cf['url'])}:{cf['lineNumber'] + 1}]")
        cur = parent.get(cur)
    stack_cache[nid] = keys
    return keys


for nid, us in self_us.items():
    for k in stack_keys(nid):
        incl[k] += us
print(f"\ntop {top_n} functions (inclusive time):")
for k, us in incl.most_common(top_n):
    print(f"  {100 * us / total:5.1f}%  {us / 1e6:7.1f} s  {k}")
