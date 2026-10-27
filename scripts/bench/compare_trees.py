#!/usr/bin/env python3
"""Compare vault folders produced by different clients: paths and content.

    compare_trees.py NAME=DIR NAME=DIR [NAME=DIR ...] [--json OUT]

The first folder is the reference. For every other folder it lists paths only
in the reference, only in the other, and present in both with different bytes.
Client bookkeeping files are skipped: lsync's `.lsync.lock` and `.nomedia`, and
Obsidian's own `.obsidian/` folder (the plug-in's isolated profile creates it).
"""
import hashlib
import json
import os
import sys

# Bookkeeping files: lsync's lock and .nomedia, the official CLI's snapshot of the vault state.
SKIP_FILES = {".lsync.lock", ".nomedia", ".livesync-snapshot.json"}
SKIP_DIRS = {".obsidian"}


def digest_tree(root):
    out = {}
    for d, dirs, files in os.walk(root):
        rel_d = os.path.relpath(d, root)
        if rel_d == ".":
            dirs[:] = [x for x in dirs if x not in SKIP_DIRS]
        for name in files:
            rel = os.path.normpath(os.path.join(rel_d, name))
            if rel in SKIP_FILES:
                continue
            p = os.path.join(d, name)
            if os.path.islink(p) or not os.path.isfile(p):
                continue
            h = hashlib.sha256()
            with open(p, "rb") as f:
                for b in iter(lambda: f.read(1 << 20), b""):
                    h.update(b)
            out[rel] = h.hexdigest()
    return out


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    out_json = None
    if "--json" in sys.argv:
        out_json = sys.argv[sys.argv.index("--json") + 1]
        args = [a for a in args if a != out_json]
    trees = []
    for a in args:
        name, _, path = a.partition("=")
        trees.append((name, digest_tree(path)))
    ref_name, ref = trees[0]
    report = {"files": {n: len(t) for n, t in trees}, "diffs": {}}
    print("files: " + ", ".join(f"{n} {len(t)}" for n, t in trees))
    for name, t in trees[1:]:
        only_ref = sorted(set(ref) - set(t))
        only_other = sorted(set(t) - set(ref))
        differ = sorted(p for p in set(ref) & set(t) if ref[p] != t[p])
        report["diffs"][name] = {f"only_{ref_name}": only_ref, f"only_{name}": only_other, "content_differs": differ}
        same = len(set(ref) & set(t)) - len(differ)
        print(f"{ref_name} vs {name}: {same} identical, {len(only_ref)} only in {ref_name}, "
              f"{len(only_other)} only in {name}, {len(differ)} differ")
        for label, items in ((f"only in {ref_name}", only_ref), (f"only in {name}", only_other), ("differ", differ)):
            for p in items[:40]:
                print(f"  {label}: {p}")
            if len(items) > 40:
                print(f"  {label}: ... {len(items) - 40} more")
    if out_json:
        with open(out_json, "w") as f:
            json.dump(report, f, indent=1)


if __name__ == "__main__":
    main()
