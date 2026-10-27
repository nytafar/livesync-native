#!/usr/bin/env python3
"""Summarise $BENCH_WORK/results.jsonl and compare the cold-sync trees.

    python3 scripts/bench/summarise.py [WORK_DIR]

Prints, per test and contender, the median (min-max) of each metric over the
repetitions, the live latencies (median and p90 over all samples), and the
differences between the trees each client produced on its cold sync.
"""
import glob
import json
import os
import statistics
import sys

work = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("BENCH_WORK", os.path.expanduser("~/scratch/lsync-bench"))
rows = [json.loads(line) for line in open(os.path.join(work, "results.jsonl")) if line.strip()]
ORDER = ["lsync", "official", "plugin"]
METRICS = {
    "cold": ["wall_s", "cpu_s", "peak_rss_mb", "peak_pss_mb", "state_mb", "net_down_mb", "net_up_mb", "server_cpu_s",
             "disk_write_mb", "vault_files", "launch_s"],
    "inc-10": ["wall_s", "cpu_s", "peak_rss_mb", "peak_pss_mb", "net_down_mb", "server_cpu_s", "disk_write_mb"],
    "inc-10-untimed": ["wall_s", "cpu_s", "peak_rss_mb", "disk_write_mb"],
    "inc-1000": ["wall_s", "cpu_s", "peak_rss_mb", "peak_pss_mb", "net_down_mb", "server_cpu_s", "disk_write_mb"],
    "idle": ["cpu_s", "mean_rss_mb", "mean_pss_mb", "ctx_switches", "disk_write_mb", "net_down_mb", "net_up_mb",
             "server_cpu_s", "state_mb", "processes"],
}


def fmt(vals):
    vals = [v for v in vals if v is not None]
    if not vals:
        return "-"
    med = statistics.median(vals)
    if len(vals) == 1:
        return f"{med:g}"
    return f"{med:g} ({min(vals):g}-{max(vals):g})"


for r in rows:
    if r.get("test") == "snapshot":
        print(f"snapshot {r['db']}: {r['doc_count']} docs, update_seq {r['update_seq']}")
        break
for test, metrics in METRICS.items():
    print(f"\n## {test}  (median (min-max) over repetitions)\n")
    print("| metric | " + " | ".join(ORDER) + " |")
    print("|---|" + "---|" * len(ORDER))
    for m in metrics:
        cells = [fmt([r.get(m) for r in rows if r.get("test") == test and r.get("contender") == c]) for c in ORDER]
        if any(x != "-" for x in cells):
            print(f"| {m} | " + " | ".join(cells) + " |")
    n = [len([r for r in rows if r.get("test") == test and r.get("contender") == c]) for c in ORDER]
    print(f"| reps | " + " | ".join(map(str, n)) + " |")

print("\n## live latency, seconds (median / p90 / max over all samples)\n")
print("| direction | " + " | ".join(ORDER) + " |")
print("|---|" + "---|" * len(ORDER))
for key, label in (("server_to_local_s", "server -> local file"), ("local_to_server_s", "local file -> server")):
    cells = []
    for c in ORDER:
        vals = [v for r in rows if r.get("test") == "live" and r.get("contender") == c for v in r.get(key, [])]
        ok = sorted(v for v in vals if v is not None)
        miss = len(vals) - len(ok)
        if not ok:
            cells.append("-")
            continue
        p90 = ok[min(len(ok) - 1, int(round(0.9 * (len(ok) - 1))))]
        cells.append(f"{statistics.median(ok):.2f} / {p90:.2f} / {ok[-1]:.2f} (n={len(ok)}{f', {miss} missed' if miss else ''})")
    print(f"| {label} | " + " | ".join(cells) + " |")

errors = [r for r in rows if r.get("test") == "error" or r.get("failed")]
if errors:
    print("\n## errors and failed tests\n")
    for r in errors:
        print(f"- {r['contender']} r{r['rep']} {r['test']}: {r.get('error') or r.get('note')}")

trees = {}
for f in sorted(glob.glob(os.path.join(work, "trees", "*.json"))):
    trees[os.path.basename(f)[:-5]] = json.load(open(f))
if trees:
    print("\n## cold-sync trees\n")
    names = sorted(trees, key=lambda n: (ORDER.index(n.split("-r")[0]) if n.split("-r")[0] in ORDER else 9, n))
    ref = names[0]
    for n in names:
        t = trees[n]
        only_ref = sorted(set(trees[ref]) - set(t))
        only_n = sorted(set(t) - set(trees[ref]))
        differ = sorted(p for p in set(t) & set(trees[ref]) if t[p] != trees[ref][p])
        print(f"- {n}: {len(t)} files; vs {ref}: {len(only_ref)} missing, {len(only_n)} extra, {len(differ)} differ")
        for label, items in (("missing", only_ref), ("extra", only_n), ("differ", differ)):
            for p in items[:15]:
                print(f"    {label}: {p}")
            if len(items) > 15:
                print(f"    {label}: ... {len(items) - 15} more")
