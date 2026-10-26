#!/usr/bin/env python3
"""Compare an lsync-pulled folder with the desktop vault (read-only on both).

For every file in the pulled folder, compare bytes with the same path under the
reference vault (default ~/hvelv) and classify:

  identical
  different    explained when the reference file changed after the copy time
               (max of mtime and ctime, since LiveSync sets mtime from the
               document when it writes), or when the index marks it conflicted
  missing      no file at that path in the reference vault; explained when the
               parent directory changed after the copy time (deleted/renamed)

Also lists the reverse direction (reference files the pull did not produce) as
information, with the same "changed after copy" test.

Only reads the reference vault. Exit status 0 when every difference and missing
file is explained.
"""
import argparse
import datetime as dt
import hashlib
import json
import os
import sqlite3
import sys


def file_digest(path):
    h = hashlib.blake2b(digest_size=16)
    with open(path, "rb") as f:
        while True:
            b = f.read(1 << 20)
            if not b:
                break
            h.update(b)
    return h.digest()


def walk_files(root, skip_dot=True):
    for dirpath, dirnames, filenames in os.walk(root):
        if skip_dot:
            dirnames[:] = [d for d in dirnames if not d.startswith(".")]
        for name in filenames:
            if skip_dot and name.startswith("."):
                continue
            full = os.path.join(dirpath, name)
            yield os.path.relpath(full, root)


def git_ignored(ref, paths):
    """Paths the reference vault's .gitignore files exclude. The desktop
    LiveSync node runs with useIgnoreFiles + ignoreFiles=.gitignore, so these
    paths are never synced from (or to) the desktop. Read-only: --no-index."""
    if not paths or not os.path.isdir(os.path.join(ref, ".git")):
        return set()
    import subprocess
    out = subprocess.run(
        ["git", "-C", ref, "check-ignore", "--no-index", "-z", "--stdin"],
        input="\0".join(paths).encode() + b"\0", capture_output=True,
    ).stdout
    return {p.decode() for p in out.split(b"\0") if p}


def symlink_ancestor(ref, rel):
    parts = rel.split("/")
    for i in range(1, len(parts)):
        anc = os.path.join(ref, *parts[:i])
        if os.path.islink(anc):
            return "/".join(parts[:i])
    return None


def changed_after(st, cutoff):
    return max(st.st_mtime, st.st_ctime) > cutoff


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pulled", help="folder produced by `lsync pull`")
    ap.add_argument("--reference", default=os.path.expanduser("~/hvelv"), help="desktop vault (read only)")
    ap.add_argument("--copy-time", default="2026-10-26 18:00", help="local time the database copy was taken")
    ap.add_argument("--index", help="lsync index.sqlite (to explain conflicted/skipped entries)")
    ap.add_argument("--max-size-mb", type=int, default=50)
    ap.add_argument("--list", type=int, default=40, help="max entries listed per category")
    ap.add_argument("--json", help="write full results as JSON to this file")
    a = ap.parse_args()

    cutoff = dt.datetime.strptime(a.copy_time, "%Y-%m-%d %H:%M").timestamp()
    states = {}
    if a.index:
        con = sqlite3.connect(f"file:{a.index}?mode=ro", uri=True)
        for path, state, note in con.execute("SELECT path, state, note FROM files"):
            states[path] = (state, note)

    res = {k: [] for k in ("identical", "different_explained", "different_unexplained",
                           "missing_explained", "missing_unexplained")}
    pulled = set()
    for rel in walk_files(a.pulled):
        pulled.add(rel)
        ref = os.path.join(a.reference, rel)
        try:
            st = os.stat(ref)
        except FileNotFoundError:
            parent = os.path.dirname(ref)
            why = None
            if os.path.isdir(parent) and changed_after(os.stat(parent), cutoff):
                why = "parent folder changed after the copy (file deleted or renamed)"
            elif not os.path.isdir(parent):
                # Walk up to the first existing ancestor.
                anc = parent
                while anc != a.reference and not os.path.isdir(anc):
                    anc = os.path.dirname(anc)
                if changed_after(os.stat(anc), cutoff):
                    why = f"folder removed after the copy ({os.path.relpath(anc, a.reference)} changed)"
            key = "missing_explained" if why else "missing_unexplained"
            res[key].append({"path": rel, "why": why})
            continue
        if file_digest(os.path.join(a.pulled, rel)) == file_digest(ref):
            res["identical"].append({"path": rel})
            continue
        when = dt.datetime.fromtimestamp(max(st.st_mtime, st.st_ctime)).strftime("%Y-%m-%d %H:%M:%S")
        if changed_after(st, cutoff):
            res["different_explained"].append({"path": rel, "why": f"reference changed {when}, after the copy"})
        elif states.get(rel, ("",))[0] == "conflicted":
            res["different_explained"].append({"path": rel, "why": f"conflicted in the database ({states[rel][1]})"})
        else:
            res["different_unexplained"].append({"path": rel, "why": f"reference last changed {when}"})

    # Missing files the desktop never syncs (its .gitignore) or that sit behind a symlink.
    still = []
    ignored = git_ignored(a.reference, [e["path"] for e in res["missing_unexplained"]])
    for e in res["missing_unexplained"]:
        link = symlink_ancestor(a.reference, e["path"])
        if e["path"] in ignored:
            e["why"] = "excluded by the reference vault's .gitignore (desktop LiveSync uses ignore files), so the desktop never received it"
            res["missing_explained"].append(e)
        elif link:
            e["why"] = f"{link} is a symlink in the reference vault"
            res["missing_explained"].append(e)
        else:
            still.append(e)
    res["missing_unexplained"] = still

    # Reverse direction: reference files the pull did not produce.
    reverse = {"after_copy": [], "too_large": [], "skipped_by_index": [], "symlink": [], "gitignored": [], "case_collision": [], "other": []}
    for rel in walk_files(a.reference):
        if rel in pulled:
            continue
        st = os.lstat(os.path.join(a.reference, rel))
        if os.path.islink(os.path.join(a.reference, rel)):
            reverse["symlink"].append(rel)
        elif changed_after(st, cutoff):
            reverse["after_copy"].append(rel)
        elif st.st_size > a.max_size_mb * 1024 * 1024:
            reverse["too_large"].append(rel)
        elif rel in states:
            reverse["skipped_by_index"].append(f"{rel} [{states[rel][0]}: {states[rel][1]}]")
        else:
            reverse["other"].append(rel)
    pulled_ci = {p.lower() for p in pulled}
    reverse["case_collision"] = [p for p in reverse["other"] if p.lower() in pulled_ci]
    reverse["other"] = [p for p in reverse["other"] if p.lower() not in pulled_ci]
    ign = git_ignored(a.reference, reverse["other"])
    reverse["gitignored"] = [p for p in reverse["other"] if p in ign]
    reverse["other"] = [p for p in reverse["other"] if p not in ign]

    total = len(pulled)
    print(f"pulled folder:   {a.pulled}")
    print(f"reference vault: {a.reference} (read only)")
    print(f"copy time:       {a.copy_time} local")
    print()
    print(f"files compared:  {total}")
    print(f"  identical      {len(res['identical'])}")
    print(f"  different      {len(res['different_explained']) + len(res['different_unexplained'])}"
          f"  (explained {len(res['different_explained'])}, unexplained {len(res['different_unexplained'])})")
    print(f"  missing        {len(res['missing_explained']) + len(res['missing_unexplained'])}"
          f"  (explained {len(res['missing_explained'])}, unexplained {len(res['missing_unexplained'])})")
    for key in ("different_explained", "different_unexplained", "missing_explained", "missing_unexplained"):
        if res[key]:
            print(f"\n{key.replace('_', ' ')}:")
            for e in res[key][: a.list]:
                print(f"  {e['path']}  — {e['why']}")
            if len(res[key]) > a.list:
                print(f"  ... and {len(res[key]) - a.list} more")
    print("\nreference files not produced by the pull (information):")
    for key, items in reverse.items():
        print(f"  {key:<17} {len(items)}")
        for p in items[: min(a.list, 10)]:
            print(f"      {p}")
    if a.json:
        with open(a.json, "w") as f:
            json.dump({"results": res, "reverse": reverse}, f, indent=1)
    unexplained = len(res["different_unexplained"]) + len(res["missing_unexplained"])
    print(f"\nunexplained: {unexplained}")
    return 0 if unexplained == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
