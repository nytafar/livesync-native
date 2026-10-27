#!/usr/bin/env python3
"""Run one command and print one JSON line of what it cost.

    measure.py --label NAME [--state DIR ...] [--vault DIR] -- cmd args...

wall_s       wall time
cpu_s        user + system CPU of the command and every descendant it waited for
user_s, sys_s
maxrss_mb    peak resident set of the largest single process in the tree
disk_write_mb  bytes written to storage by the tree (ru_oublock)
net_tx_mb    bytes the dev CouchDB container sent (what the client downloaded)
net_rx_mb    bytes it received (what the client uploaded)
server_cpu_s CPU the CouchDB container used meanwhile
state_mb     bytes on disk under each --state dir afterwards (du --apparent-size
             would hide preallocation; this uses allocated blocks, like du)
vault_files  regular files under --vault afterwards, vault_mb their size
rc           exit status

The network and server numbers are container-wide, so nothing else may talk to
the dev container while a measured run is in flight.
"""
import argparse
import json
import os
import resource
import subprocess
import sys
import time

CONTAINER = os.environ.get("DEV_CONTAINER", "lsync-dev-couchdb")


def net():
    out = subprocess.run(
        ["docker", "exec", CONTAINER, "cat", "/sys/class/net/eth0/statistics/tx_bytes",
         "/sys/class/net/eth0/statistics/rx_bytes"], capture_output=True, text=True, check=True).stdout.split()
    return int(out[0]), int(out[1])


def server_cpu():
    cid = subprocess.run(["docker", "inspect", "-f", "{{.Id}}", CONTAINER], capture_output=True, text=True,
                         check=True).stdout.strip()
    for f in (f"/sys/fs/cgroup/system.slice/docker-{cid}.scope/cpu.stat", f"/sys/fs/cgroup/docker/{cid}/cpu.stat"):
        if os.path.exists(f):
            for line in open(f):
                k, v = line.split()
                if k == "usage_usec":
                    return int(v) / 1e6
    return float("nan")


def du(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.lstat(os.path.join(root, name)).st_blocks * 512
            except FileNotFoundError:
                pass
    return total


def tree(path):
    n = size = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            p = os.path.join(root, name)
            if os.path.isfile(p) and not os.path.islink(p):
                n += 1
                size += os.path.getsize(p)
    return n, size


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True)
    ap.add_argument("--state", action="append", default=[])
    ap.add_argument("--vault")
    ap.add_argument("--log")
    ap.add_argument("--stamp", action="store_true", help="prefix each log line with seconds since start")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    n0, c0 = net(), server_cpu()
    r0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    t0 = time.monotonic()
    log = open(a.log, "ab") if a.log else subprocess.DEVNULL
    if a.stamp and a.log:
        p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        for line in p.stdout:
            log.write(f"{time.monotonic() - t0:9.2f} ".encode() + line)
        rc = p.wait()
    else:
        rc = subprocess.call(cmd, stdout=log, stderr=log)
    wall = time.monotonic() - t0
    r1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    n1, c1 = net(), server_cpu()
    out = {
        "label": a.label, "rc": rc, "wall_s": round(wall, 2),
        "user_s": round(r1.ru_utime - r0.ru_utime, 2), "sys_s": round(r1.ru_stime - r0.ru_stime, 2),
        "cpu_s": round((r1.ru_utime - r0.ru_utime) + (r1.ru_stime - r0.ru_stime), 2),
        # ru_maxrss of RUSAGE_CHILDREN is the largest descendant so far, so measure.py
        # must run once per command (the scripts start a fresh one each time).
        "maxrss_mb": round(r1.ru_maxrss / 1024, 1),
        # Blocks written to storage by the tree (512-byte units).
        "disk_write_mb": round((r1.ru_oublock - r0.ru_oublock) * 512 / 1e6, 1),
        "net_tx_mb": round((n1[0] - n0[0]) / 1e6, 2), "net_rx_mb": round((n1[1] - n0[1]) / 1e6, 2),
        "server_cpu_s": round(c1 - c0, 2),
        "state_mb": round(sum(du(s) for s in a.state) / 1e6, 1),
        "ts": time.strftime("%Y-%m-%dT%H:%M:%S"),
    }
    if a.vault:
        n, size = tree(a.vault)
        out["vault_files"], out["vault_mb"] = n, round(size / 1e6, 1)
    print(json.dumps(out), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
